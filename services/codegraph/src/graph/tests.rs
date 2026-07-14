//! End-to-end tests over the `graph` module's public pipeline (`extract_file` + `resolve`) — the
//! contract the rest of the crate (and `agent-runner`) depends on, not the internals of any one
//! submodule. Kept alongside [`super`] rather than split per submodule because every test here drives
//! the whole per-file-extraction → cross-file-resolution flow, mirroring real usage.

use super::*;
use crate::lang;

fn graph_of_lang(language: &str, files: &[(&str, &str)]) -> Graph {
    let facts: Vec<FileSymbols> = files
        .iter()
        .map(|(path, src)| {
            let tree = lang::parse(src, language).expect("source parses");
            extract_file(&tree, path, src, language)
        })
        .collect();
    resolve(facts)
}

fn graph_of(files: &[(&str, &str)]) -> Graph {
    graph_of_lang("rust", files)
}

/// Find the single node with `label`, panicking with the graph on failure (test ergonomics).
fn node<'a>(g: &'a Graph, label: &str) -> &'a GraphNode {
    g.nodes
        .iter()
        .find(|n| n.label == label)
        .unwrap_or_else(|| panic!("no node labelled {label:?}; nodes = {:?}", g.nodes))
}

/// True iff a `calls` edge `caller → callee` (by label) exists.
fn has_call(g: &Graph, caller: &str, callee: &str) -> bool {
    let src = node(g, caller).node_id.as_str();
    let dst = node(g, callee).node_id.as_str();
    g.edges
        .iter()
        .any(|e| e.relation == "calls" && e.source == src && e.target == dst)
}

#[test]
fn emits_file_and_function_nodes_with_contains() {
    let g = graph_of(&[("src/math.rs", "fn add(a: i32, b: i32) -> i32 { a + b }\n")]);
    assert!(
        g.nodes
            .iter()
            .any(|n| n.node_id == "src/math.rs" && n.label == "math.rs")
    );
    let add = g
        .nodes
        .iter()
        .find(|n| n.label == "add()")
        .expect("add node");
    assert_eq!(add.source_file, "src/math.rs");
    assert!(
        g.edges.iter().any(|e| e.relation == "contains"
            && e.source == "src/math.rs"
            && e.target == add.node_id),
        "file should contain add"
    );
}

#[test]
fn intra_file_call_edge_is_built() {
    let src = "fn helper() {}\nfn caller() { helper(); }\n";
    let g = graph_of(&[("src/a.rs", src)]);
    let helper = g.nodes.iter().find(|n| n.label == "helper()").unwrap();
    let caller = g.nodes.iter().find(|n| n.label == "caller()").unwrap();
    assert!(
        g.edges.iter().any(|e| e.relation == "calls"
            && e.source == caller.node_id
            && e.target == helper.node_id),
        "caller → helper calls edge, got {:?}",
        g.edges
    );
}

#[test]
fn cross_file_call_resolves_to_definition_in_another_file() {
    // The high-value case (ADR-0086): a reference in file A → a definition in file B.
    let a = ("src/a.rs", "fn caller() { target(); }\n");
    let b = ("src/b.rs", "fn target() {}\n");
    let g = graph_of(&[a, b]);
    let target = g.nodes.iter().find(|n| n.label == "target()").unwrap();
    let caller = g.nodes.iter().find(|n| n.label == "caller()").unwrap();
    assert_eq!(target.source_file, "src/b.rs");
    assert_eq!(caller.source_file, "src/a.rs");
    assert!(
        g.edges.iter().any(|e| e.relation == "calls"
            && e.source == caller.node_id
            && e.target == target.node_id),
        "cross-file caller → target edge missing; edges = {:?}",
        g.edges
    );
}

#[test]
fn method_call_resolves_to_method_definition() {
    let src = "struct S;\nimpl S { fn run(&self) {} }\nfn go(s: &S) { s.run(); }\n";
    let g = graph_of(&[("src/s.rs", src)]);
    let run = g.nodes.iter().find(|n| n.label == "run()").unwrap();
    let go = g.nodes.iter().find(|n| n.label == "go()").unwrap();
    assert!(
        g.edges
            .iter()
            .any(|e| e.relation == "calls" && e.source == go.node_id && e.target == run.node_id),
        "go → S::run method-call edge missing; edges = {:?}",
        g.edges
    );
}

#[test]
fn ambiguous_global_name_is_not_guessed() {
    // Two files each define `dup`; a third calls `dup` with no local def → ambiguous → no edge.
    let files = &[
        ("src/x.rs", "fn dup() {}\n"),
        ("src/y.rs", "fn dup() {}\n"),
        ("src/z.rs", "fn caller() { dup(); }\n"),
    ];
    let g = graph_of(files);
    assert!(
        !g.edges.iter().any(|e| e.relation == "calls"),
        "ambiguous name must not produce a guessed calls edge, got {:?}",
        g.edges
    );
}

#[test]
fn method_definition_emits_a_method_edge_not_contains() {
    // Parity with Graphify: a callable inside a type container is joined by `method`, not
    // `contains`.
    let src = "struct S;\nimpl S { fn run(&self) {} }\n";
    let g = graph_of(&[("src/s.rs", src)]);
    let run = g.nodes.iter().find(|n| n.label == "run()").unwrap();
    // The enclosing container is the `impl S` node (codegraph emits an anonymous `impl` node, not
    // the named type — this asserts codegraph's own SELF-CONSISTENCY: the `method` edge's source
    // is exactly that container node. Whether Graphify anchors `method` at the type name instead
    // is a separate side-by-side parity check still open on the #360 gate.
    let impl_node = g
        .nodes
        .iter()
        .find(|n| n.label == "impl")
        .expect("impl container node");
    let method_edge = g
        .edges
        .iter()
        .find(|e| e.relation == "method" && e.target == run.node_id)
        .unwrap_or_else(|| panic!("S::run must be a `method` edge; edges = {:?}", g.edges));
    assert_eq!(
        method_edge.source, impl_node.node_id,
        "method edge must originate from the `impl S` container node, not elsewhere"
    );
    assert!(
        !g.edges
            .iter()
            .any(|e| e.relation == "contains" && e.target == run.node_id),
        "S::run must NOT also be a `contains` target; edges = {:?}",
        g.edges
    );
}

#[test]
fn ambiguous_local_does_not_shadow_a_qualified_global() {
    // file a: two colliding `new`s + a call to `C::new()` (a *different* type). The local set is
    // ambiguous, but the qualifier singles out the global `C::new` in file c — the edge must
    // resolve cross-file, not be dropped (P2-c).
    let a = (
        "src/a.rs",
        "\
struct A;
struct B;
impl A { fn new() -> A { A } }
impl B { fn new() -> B { B } }
fn make() { let _ = C::new(); }
",
    );
    let c = ("src/c.rs", "struct C;\nimpl C { fn new() -> C { C } }\n");
    let g = graph_of(&[a, c]);
    let make = g.nodes.iter().find(|n| n.label == "make()").unwrap();
    let c_new = g
        .nodes
        .iter()
        .find(|n| n.label == "new()" && n.source_file == "src/c.rs")
        .expect("C::new in c.rs");
    let calls: Vec<_> = g
        .edges
        .iter()
        .filter(|e| e.relation == "calls" && e.source == make.node_id)
        .collect();
    assert_eq!(calls.len(), 1, "one cross-file edge; got {calls:?}");
    assert_eq!(
        calls[0].target, c_new.node_id,
        "ambiguous locals must not shadow the qualified global C::new"
    );
}

#[test]
fn labels_and_lines_match_graphify_shape() {
    // 1-based lines + `()` on callables (Graphify parity).
    let g = graph_of(&[("src/m.rs", "\nfn add() {}\n")]); // `add` on source line 2
    let add = g.nodes.iter().find(|n| n.label == "add()").unwrap();
    assert_eq!(add.start_line, 2, "1-based start_line");
    let file = g.nodes.iter().find(|n| n.node_id == "src/m.rs").unwrap();
    assert_eq!(file.start_line, 1, "file node is line 1 like Graphify's L1");
}

#[test]
fn same_file_qualified_call_hits_the_right_constructor() {
    // Two impls in one file both define `new`. `A::new()` must resolve to A's `new` only — the
    // old same-file branch fanned out to BOTH (the bug this fixes).
    let src = "\
struct A;
struct B;
impl A { fn new() -> A { A } }
impl B { fn new() -> B { B } }
fn make() { let _ = A::new(); }
";
    let g = graph_of(&[("src/lib.rs", src)]);
    let make = g.nodes.iter().find(|n| n.label == "make()").unwrap();
    // `impl A { fn new ... }` are one source line: A::new is line 3, B::new line 4.
    let a_new = g
        .nodes
        .iter()
        .find(|n| n.label == "new()" && n.start_line == 3)
        .expect("A::new at line 3");
    let b_new = g
        .nodes
        .iter()
        .find(|n| n.label == "new()" && n.start_line == 4)
        .expect("B::new at line 4");
    let calls: Vec<_> = g
        .edges
        .iter()
        .filter(|e| e.relation == "calls" && e.source == make.node_id)
        .collect();
    assert_eq!(
        calls.len(),
        1,
        "exactly one calls edge (no fan-out); got {calls:?}"
    );
    assert_eq!(calls[0].target, a_new.node_id, "must resolve to A::new");
    assert_ne!(calls[0].target, b_new.node_id, "must NOT also hit B::new");
}

#[test]
fn same_file_bare_duplicate_call_is_dropped_not_fanned_out() {
    // `default` on two impls; a *bare* `default()` can't be attributed → dropped, not one edge
    // to each definition.
    let src = "\
struct A;
struct B;
impl A { fn default() -> A { A } }
impl B { fn default() -> B { B } }
fn make() { let _ = default(); }
";
    let g = graph_of(&[("src/lib.rs", src)]);
    assert!(
        !g.edges.iter().any(|e| e.relation == "calls"),
        "ambiguous bare call must be dropped, not fanned out; edges = {:?}",
        g.edges
    );
}

#[test]
fn qualified_call_disambiguates_from_and_build_across_impls() {
    // A mix of `from`/`build` constructors on colliding types; each qualified call lands on its
    // own type's method and nothing else.
    let src = "\
struct Meters;
struct Feet;
impl Meters { fn from(v: i32) -> Meters { Meters } fn build() -> Meters { Meters } }
impl Feet { fn from(v: i32) -> Feet { Feet } fn build() -> Feet { Feet } }
fn go() { let _ = Feet::from(3); let _ = Meters::build(); }
";
    let g = graph_of(&[("src/u.rs", src)]);
    let go = g.nodes.iter().find(|n| n.label == "go()").unwrap();
    let calls: Vec<_> = g
        .edges
        .iter()
        .filter(|e| e.relation == "calls" && e.source == go.node_id)
        .collect();
    assert_eq!(calls.len(), 2, "two resolved edges; got {calls:?}");
    // `Feet::from` and `Meters::build` are the only correct targets.
    // `impl Meters {...}` is line 3, `impl Feet {...}` line 4 (each impl + its methods one line).
    let feet_from = g
        .nodes
        .iter()
        .find(|n| n.label == "from()" && n.start_line == 4)
        .expect("Feet::from at line 4");
    let meters_build = g
        .nodes
        .iter()
        .find(|n| n.label == "build()" && n.start_line == 3)
        .expect("Meters::build at line 3");
    assert!(calls.iter().any(|e| e.target == feet_from.node_id));
    assert!(calls.iter().any(|e| e.target == meters_build.node_id));
}

#[test]
fn output_is_deterministic() {
    let files = &[
        ("src/b.rs", "fn target() {}\n"),
        ("src/a.rs", "fn caller() { target(); }\n"),
    ];
    let g1 = graph_of(files);
    let g2 = graph_of(files);
    assert_eq!(g1, g2, "resolve must be order-independent + canonical");
}

// ── Python ──────────────────────────────────────────────────────────────────────────────────

#[test]
fn python_free_function_contains_and_intra_file_call() {
    let src = "def helper():\n    pass\n\ndef caller():\n    helper()\n";
    let g = graph_of_lang("python", &[("app/util.py", src)]);
    let file = node(&g, "util.py");
    assert_eq!(file.node_id, "app/util.py");
    let helper = node(&g, "helper()");
    assert_eq!(helper.start_line, 1, "1-based def line");
    assert!(
        g.edges.iter().any(|e| e.relation == "contains"
            && e.source == "app/util.py"
            && e.target == helper.node_id),
        "file → helper contains edge; edges = {:?}",
        g.edges
    );
    assert!(has_call(&g, "caller()", "helper()"), "caller → helper");
}

#[test]
fn python_class_method_emits_method_edge_and_self_call_resolves() {
    // A class with two methods; `describe` calls `self.area()`. `area` is a `method` edge off the
    // class node, and the self-call resolves to the single `area` despite the `self` receiver.
    let src = "\
class Circle:
    def area(self):
        return 3.14

    def describe(self):
        return self.area()
";
    let g = graph_of_lang("python", &[("shapes.py", src)]);
    let class = node(&g, "Circle");
    let area = node(&g, "area()");
    assert!(
        g.edges.iter().any(|e| e.relation == "method"
            && e.source == class.node_id
            && e.target == area.node_id),
        "Circle → area must be a `method` edge; edges = {:?}",
        g.edges
    );
    assert!(
        !g.edges
            .iter()
            .any(|e| e.relation == "contains" && e.target == area.node_id),
        "a method must not also be a `contains` target"
    );
    assert!(
        has_call(&g, "describe()", "area()"),
        "self.area() must resolve to Circle.area; edges = {:?}",
        g.edges
    );
}

#[test]
fn python_cross_file_call_resolves() {
    let a = ("pkg/a.py", "def caller():\n    target()\n");
    let b = ("pkg/b.py", "def target():\n    pass\n");
    let g = graph_of_lang("python", &[a, b]);
    assert_eq!(node(&g, "target()").source_file, "pkg/b.py");
    assert!(
        has_call(&g, "caller()", "target()"),
        "cross-file caller → target"
    );
}

#[test]
fn python_colliding_method_names_qualified_resolves_bare_dropped() {
    // Two classes each define `new`; `Circle.new()` resolves to Circle's only, a bare `new()`
    // is ambiguous and dropped (never fanned out to both).
    let src = "\
class Circle:
    def new(self):
        return self

class Square:
    def new(self):
        return self

def build():
    return Circle.new()

def build_bare():
    return new()
";
    let g = graph_of_lang("python", &[("shapes.py", src)]);
    let circle_new = g
        .nodes
        .iter()
        .find(|n| n.label == "new()" && n.start_line == 2)
        .expect("Circle.new at line 2");
    let build_calls: Vec<_> = g
        .edges
        .iter()
        .filter(|e| e.relation == "calls" && e.source == node(&g, "build()").node_id)
        .collect();
    assert_eq!(
        build_calls.len(),
        1,
        "one qualified edge; got {build_calls:?}"
    );
    assert_eq!(
        build_calls[0].target, circle_new.node_id,
        "→ Circle.new only"
    );
    assert!(
        !g.edges
            .iter()
            .any(|e| e.relation == "calls" && e.source == node(&g, "build_bare()").node_id),
        "bare ambiguous new() must be dropped; edges = {:?}",
        g.edges
    );
}

#[test]
fn python_decorated_def_is_named_not_wrapped() {
    // `@decorator` must not spawn an anonymous wrapper node — the def keeps its real name/line.
    let src = "import functools\n\n@functools.cache\ndef cached():\n    return 1\n";
    let g = graph_of_lang("python", &[("m.py", src)]);
    let cached = node(&g, "cached()");
    assert_eq!(cached.start_line, 4, "def line, not the decorator line");
    assert!(
        g.edges
            .iter()
            .any(|e| e.relation == "contains" && e.source == "m.py" && e.target == cached.node_id),
        "decorated def is contained by the file directly (no wrapper); edges = {:?}",
        g.edges
    );
    assert!(
        !g.nodes.iter().any(|n| n.label == "function"),
        "no anonymous `function` wrapper node; nodes = {:?}",
        g.nodes
    );
}

// ── TypeScript / JavaScript ─────────────────────────────────────────────────────────────────

#[test]
fn ts_function_declaration_and_call() {
    let src = "function helper() {}\nfunction caller() { helper(); }\n";
    let g = graph_of_lang("typescript", &[("src/a.ts", src)]);
    assert!(has_call(&g, "caller()", "helper()"), "caller → helper");
}

#[test]
fn ts_named_arrow_const_is_a_callable() {
    // The dominant modern form: `const helper = () => {}`. It must be a named, callable def.
    let src = "const helper = () => {};\nfunction caller() { helper(); }\n";
    let g = graph_of_lang("typescript", &[("src/a.ts", src)]);
    let helper = node(&g, "helper()");
    assert!(
        g.edges.iter().any(|e| e.relation == "contains"
            && e.source == "src/a.ts"
            && e.target == helper.node_id),
        "file → const-arrow helper; edges = {:?}",
        g.edges
    );
    assert!(
        has_call(&g, "caller()", "helper()"),
        "caller → const-arrow helper"
    );
}

#[test]
fn ts_class_method_edge_and_this_call() {
    let src = "\
class Service {
  run() {}
  go() { this.run(); }
}
";
    let g = graph_of_lang("typescript", &[("src/s.ts", src)]);
    let class = node(&g, "Service");
    let run = node(&g, "run()");
    assert!(
        g.edges.iter().any(|e| e.relation == "method"
            && e.source == class.node_id
            && e.target == run.node_id),
        "Service → run `method` edge; edges = {:?}",
        g.edges
    );
    assert!(
        has_call(&g, "go()", "run()"),
        "this.run() resolves to Service.run"
    );
}

#[test]
fn ts_cross_file_call_resolves() {
    let a = (
        "src/a.ts",
        "import { target } from './b';\nfunction caller() { target(); }\n",
    );
    let b = ("src/b.ts", "export function target() {}\n");
    let g = graph_of_lang("typescript", &[a, b]);
    assert_eq!(node(&g, "target()").source_file, "src/b.ts");
    assert!(
        has_call(&g, "caller()", "target()"),
        "cross-file caller → target"
    );
}

#[test]
fn ts_colliding_methods_qualified_resolves_bare_dropped() {
    let src = "\
class Circle { make() { return this; } }
class Square { make() { return this; } }
function build() { return Circle.make(); }
function buildBare(m) { return make(); }
";
    let g = graph_of_lang("typescript", &[("src/shapes.ts", src)]);
    let circle_make = g
        .nodes
        .iter()
        .find(|n| n.label == "make()" && n.start_line == 1)
        .expect("Circle.make at line 1");
    let build_calls: Vec<_> = g
        .edges
        .iter()
        .filter(|e| e.relation == "calls" && e.source == node(&g, "build()").node_id)
        .collect();
    assert_eq!(
        build_calls.len(),
        1,
        "one qualified edge; got {build_calls:?}"
    );
    assert_eq!(
        build_calls[0].target, circle_make.node_id,
        "→ Circle.make only"
    );
    assert!(
        !g.edges
            .iter()
            .any(|e| e.relation == "calls" && e.source == node(&g, "buildBare()").node_id),
        "bare ambiguous make() must be dropped; edges = {:?}",
        g.edges
    );
}

#[test]
fn tsx_component_uses_the_jsx_grammar() {
    // A `.tsx` component returning JSX must parse via the TSX grammar (the plain TS grammar chokes
    // on JSX). Assert the component + a hook call resolve, proving the JSX body didn't wreck the
    // parse.
    let src = "\
function useThing() { return 1; }
export const Widget = () => {
  const n = useThing();
  return <div>{n}</div>;
};
";
    let g = graph_of_lang("tsx", &[("src/Widget.tsx", src)]);
    assert!(
        has_call(&g, "Widget()", "useThing()"),
        "Widget → useThing across a JSX body; edges = {:?}",
        g.edges
    );
}

#[test]
fn javascript_jsx_arrow_component_is_extracted() {
    // JSX in a `.jsx`/`.js` file must still parse + yield the named component and its call.
    let src = "\
const Button = () => { return null; };
function App() { return Button(); }
";
    let g = graph_of_lang("javascript", &[("src/app.jsx", src)]);
    assert!(
        has_call(&g, "App()", "Button()"),
        "App → Button; edges = {:?}",
        g.edges
    );
}

// ── Java (stretch) ──────────────────────────────────────────────────────────────────────────

#[test]
fn java_class_method_edge_and_this_call() {
    let src = "\
class Greeter {
    String hello() { return \"hi\"; }
    String greet() { return this.hello(); }
}
";
    let g = graph_of_lang("java", &[("Greeter.java", src)]);
    let class = node(&g, "Greeter");
    let hello = node(&g, "hello()");
    assert!(
        g.edges.iter().any(|e| e.relation == "method"
            && e.source == class.node_id
            && e.target == hello.node_id),
        "Greeter → hello `method` edge; edges = {:?}",
        g.edges
    );
    assert!(
        has_call(&g, "greet()", "hello()"),
        "this.hello() → Greeter.hello"
    );
}

#[test]
fn java_cross_file_qualified_static_call_resolves() {
    let a = (
        "Caller.java",
        "class Caller { void run() { Helper.help(); } }\n",
    );
    let b = ("Helper.java", "class Helper { static void help() {} }\n");
    let g = graph_of_lang("java", &[a, b]);
    assert_eq!(node(&g, "help()").source_file, "Helper.java");
    assert!(
        has_call(&g, "run()", "help()"),
        "Helper.help() resolves cross-file"
    );
}

#[test]
fn java_colliding_methods_qualified_resolves() {
    let src = "\
class Circle { Circle make() { return this; } }
class Square { Square make() { return this; } }
class Builder { Object build() { return Circle.make(); } }
";
    let g = graph_of_lang("java", &[("Shapes.java", src)]);
    let circle_make = g
        .nodes
        .iter()
        .find(|n| n.label == "make()" && n.start_line == 1)
        .expect("Circle.make at line 1");
    let build_calls: Vec<_> = g
        .edges
        .iter()
        .filter(|e| e.relation == "calls" && e.source == node(&g, "build()").node_id)
        .collect();
    assert_eq!(
        build_calls.len(),
        1,
        "one qualified edge; got {build_calls:?}"
    );
    assert_eq!(
        build_calls[0].target, circle_make.node_id,
        "→ Circle.make only"
    );
}

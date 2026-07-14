//! Callee-reference extraction: given an AST node identified as a call site, recover its bare callee
//! name plus, when the call is qualified by a receiver/path (`A::new`, `Foo.bar`), the qualifier used
//! solely as an ambiguity tiebreaker by [`super::resolve::pick`]. Two independent sources feed the same
//! [`CalleeRef`] shape: the Rust `call_expression` navigation ([`callee_ref_of`]), and the tags-captured
//! callee name node for every other language ([`qualifier_from_callee_node`] — tags identify *that* a
//! node is a callee but drop the qualifier, so it is recovered here from the node's tree position).

use tree_sitter::Node;

/// The callee of a Rust `call_expression`: its bare name plus, for a qualified path, the qualifier
/// segment. `A::new()` → `{name: "new", qualifier: Some("A")}`; `a::b::foo()` → `{"foo", Some("b")}`;
/// `foo()` / `x.foo()` → no qualifier (a method receiver's type is not known without inference).
pub(super) struct CalleeRef {
    pub(super) name: String,
    pub(super) qualifier: Option<String>,
}

pub(super) fn callee_ref_of(call: &Node<'_>, bytes: &[u8]) -> Option<CalleeRef> {
    let func = call.child_by_field_name("function")?;
    callee_ref(&func, bytes)
}

fn callee_ref(node: &Node<'_>, bytes: &[u8]) -> Option<CalleeRef> {
    match node.kind() {
        "identifier" | "field_identifier" | "type_identifier" => Some(CalleeRef {
            name: text(node, bytes)?,
            qualifier: None,
        }),
        // `a::b::foo` — callable is the final `name`; qualifier is the segment before it (`b`).
        "scoped_identifier" => {
            let name = node
                .child_by_field_name("name")
                .and_then(|n| text(&n, bytes))?;
            let qualifier = node
                .child_by_field_name("path")
                .and_then(|p| path_tail(&p, bytes));
            Some(CalleeRef { name, qualifier })
        }
        // `x.foo` — the method name is the `field`; the receiver type is unknown, so no qualifier.
        "field_expression" => node
            .child_by_field_name("field")
            .and_then(|n| callee_ref(&n, bytes)),
        // `foo::<T>` — the callable is under the `function` field.
        "generic_function" => node
            .child_by_field_name("function")
            .and_then(|n| callee_ref(&n, bytes)),
        _ => None,
    }
}

/// The last segment of a path node — the receiver/namespace qualifier (`A` in `A`, `b` in `a::b`).
fn path_tail(node: &Node<'_>, bytes: &[u8]) -> Option<String> {
    match node.kind() {
        "scoped_identifier" => node
            .child_by_field_name("name")
            .and_then(|n| text(&n, bytes)),
        _ => text(node, bytes),
    }
}

/// The implementing type of an `impl_item` (`impl S` / `impl T for S` / `impl Vec<T>` → `S` / `Vec`),
/// used to scope the methods it contains for same-name disambiguation.
pub(super) fn impl_type_name(node: &Node<'_>, bytes: &[u8]) -> Option<String> {
    let ty = node.child_by_field_name("type")?;
    type_head_name(&ty, bytes)
}

fn type_head_name(node: &Node<'_>, bytes: &[u8]) -> Option<String> {
    match node.kind() {
        "type_identifier" => text(node, bytes),
        "generic_type" => node
            .child_by_field_name("type")
            .and_then(|n| type_head_name(&n, bytes)),
        "scoped_type_identifier" => node
            .child_by_field_name("name")
            .and_then(|n| text(&n, bytes)),
        _ => None,
    }
}

/// The qualifier for a tags-captured callee name node: its receiver, when the name is the property of
/// a member access (`Foo.bar()` → `Foo`; Python `obj.m()` / Java `Obj.m()`). A bare call
/// (`function`-field identifier) or a `self`/`this`/`cls`/`super` receiver yields no qualifier — the
/// call resolves on the bare name (single hit) or is dropped as ambiguous, never mis-attributed.
pub(super) fn qualifier_from_callee_node(name_node: &Node<'_>, bytes: &[u8]) -> Option<String> {
    let parent = name_node.parent()?;
    match parent.kind() {
        // JS/TS `obj.foo()`, Python `obj.foo()`, Java `obj.foo()` — the receiver is the `object` field.
        "member_expression" | "attribute" | "method_invocation" => parent
            .child_by_field_name("object")
            .and_then(|object| receiver_qualifier(&object, bytes)),
        _ => None,
    }
}

/// A receiver object as a qualifier, iff it is a plain identifier that could name a *type* (`Foo` in
/// `Foo.bar()`). Implicit-`self` receivers (`self`/`cls`/`this`/`super`) carry no type information, so
/// they yield no qualifier — the call resolves on the bare method name (single hit) or is dropped as
/// ambiguous, never mis-attributed by a bogus `self` qualifier.
fn receiver_qualifier(object: &Node<'_>, bytes: &[u8]) -> Option<String> {
    if object.kind() != "identifier" {
        return None;
    }
    let name = text(object, bytes)?;
    match name.as_str() {
        "self" | "cls" | "this" | "super" => None,
        _ => Some(name),
    }
}

fn text(node: &Node<'_>, bytes: &[u8]) -> Option<String> {
    std::str::from_utf8(&bytes[node.byte_range()])
        .ok()
        .map(str::to_string)
}

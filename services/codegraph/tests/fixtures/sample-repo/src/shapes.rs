//! Fixture exercising inherent impls, a trait impl, methods, and colliding constructor names so the
//! committed golden actually covers the graph's method/`impl` resolution (not just free functions).

struct Circle {
    radius: f64,
}

impl Circle {
    fn new(radius: f64) -> Circle {
        Circle { radius }
    }

    fn area(&self) -> f64 {
        3.14 * self.radius * self.radius
    }
}

struct Square {
    side: f64,
}

impl Square {
    // Colliding constructor name with `Circle::new` — a bare `new()` here would be ambiguous, but a
    // qualified `Circle::new(..)` call below must resolve to Circle's constructor only.
    fn new(side: f64) -> Square {
        Square { side }
    }
}

trait Shape {
    fn describe(&self) -> f64;
}

impl Shape for Circle {
    fn describe(&self) -> f64 {
        self.area()
    }
}

fn make_unit_circle() -> Circle {
    Circle::new(1.0)
}

//! Tiny fixture used by tests and docs. Intentionally trivial.

pub fn add(a: i32, b: i32) -> i32 {
    a + b
}

pub fn double(x: i32) -> i32 {
    add(x, x)
}

pub fn quadruple(x: i32) -> i32 {
    double(double(x))
}

pub fn useless() {}

pub struct Counter {
    value: i32,
}

pub trait Tick {
    fn tick(&mut self);
}

impl Tick for Counter {
    fn tick(&mut self) {
        self.value = add(self.value, 1);
    }
}

pub fn countdown(n: i32) -> i32 {
    if n <= 0 {
        0
    } else {
        countdown(n - 1)
    }
}

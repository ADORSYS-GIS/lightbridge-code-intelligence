/// Add two integers.
pub fn add(a: i32, b: i32) -> i32 {
    a + b
}

/// Print a result, delegating to the local logger.
pub fn print_result(value: i32) {
    log(value);
}

fn log(value: i32) {
    let _ = value;
}

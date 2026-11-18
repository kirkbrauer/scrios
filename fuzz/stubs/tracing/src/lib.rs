//! Just enough of `tracing` for the modules of scrios that are fuzzed: the arguments are formatted (so a panic in
//! a `Display` or a bad format string would show) and the message is dropped.

#[macro_export]
macro_rules! warn { ($($t:tt)*) => {{ let _ = format!($($t)*); }} }
#[macro_export]
macro_rules! info { ($($t:tt)*) => {{ let _ = format!($($t)*); }} }
#[macro_export]
macro_rules! debug { ($($t:tt)*) => {{ let _ = format!($($t)*); }} }

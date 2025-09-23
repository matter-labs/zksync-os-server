//! Identical to `tracing::{trace,debug,info,warn,error}` macros but:
//!   * Default target is derived via `transformed_module_path!` which replaces `zksync_os_` with `zk::`.
//!   * No support for `name:` or `parent:` fields (purely for brevity, can be added in the future if needed)
//!
//! Boilerplate was copied directly from `tracing` macros because they support a lot of ways to invoke them.

#[macro_export]
macro_rules! trace {
    // Target.
    (target: $target:expr, { $($field:tt)* }, $($arg:tt)* ) => (
        $crate::__tracing::event!(target: $target, $crate::__tracing::Level::TRACE, { $($field)* }, $($arg)*)
    );
    (target: $target:expr, $($k:ident).+ $($field:tt)* ) => (
        $crate::__tracing::event!(target: $target, $crate::__tracing::Level::TRACE, { $($k).+ $($field)* })
    );
    (target: $target:expr, ?$($k:ident).+ $($field:tt)* ) => (
        $crate::__tracing::event!(target: $target, $crate::__tracing::Level::TRACE, { ?$($k).+ $($field)* })
    );
    (target: $target:expr, %$($k:ident).+ $($field:tt)* ) => (
        $crate::__tracing::event!(target: $target, $crate::__tracing::Level::TRACE, { %$($k).+ $($field)* })
    );
    (target: $target:expr, $($arg:tt)+ ) => (
        $crate::__tracing::event!(target: $target, $crate::__tracing::Level::TRACE, {}, $($arg)+)
    );

    // ...
    ({ $($field:tt)+ }, $($arg:tt)+ ) => (
        $crate::__tracing::event!(
            target: $crate::transformed_module_path!(),
            $crate::__tracing::Level::TRACE,
            { $($field)+ },
            $($arg)+
        )
    );
    ($($k:ident).+ = $($field:tt)*) => (
        $crate::__tracing::event!(
            target: $crate::transformed_module_path!(),
            $crate::__tracing::Level::TRACE,
            { $($k).+ = $($field)*}
        )
    );
    (?$($k:ident).+ = $($field:tt)*) => (
        $crate::__tracing::event!(
            target: $crate::transformed_module_path!(),
            $crate::__tracing::Level::TRACE,
            { ?$($k).+ = $($field)*}
        )
    );
    (%$($k:ident).+ = $($field:tt)*) => (
        $crate::__tracing::event!(
            target: $crate::transformed_module_path!(),
            $crate::__tracing::Level::TRACE,
            { %$($k).+ = $($field)*}
        )
    );
    ($($k:ident).+, $($field:tt)*) => (
        $crate::__tracing::event!(
            target: $crate::transformed_module_path!(),
            $crate::__tracing::Level::TRACE,
            { $($k).+, $($field)*}
        )
    );
    (?$($k:ident).+, $($field:tt)*) => (
        $crate::__tracing::event!(
            target: $crate::transformed_module_path!(),
            $crate::__tracing::Level::TRACE,
            { ?$($k).+, $($field)*}
        )
    );
    (%$($k:ident).+, $($field:tt)*) => (
        $crate::__tracing::event!(
            target: $crate::transformed_module_path!(),
            $crate::__tracing::Level::TRACE,
            { %$($k).+, $($field)*}
        )
    );
    (?$($k:ident).+) => (
        $crate::__tracing::event!(
            target: $crate::transformed_module_path!(),
            $crate::__tracing::Level::TRACE,
            { ?$($k).+ }
        )
    );
    (%$($k:ident).+) => (
        $crate::__tracing::event!(
            target: $crate::transformed_module_path!(),
            $crate::__tracing::Level::TRACE,
            { %$($k).+ }
        )
    );
    ($($k:ident).+) => (
        $crate::__tracing::event!(
            target: $crate::transformed_module_path!(),
            $crate::__tracing::Level::TRACE,
            { $($k).+ }
        )
    );
    ($($arg:tt)+) => (
        $crate::__tracing::event!(
            target: $crate::transformed_module_path!(),
            $crate::__tracing::Level::TRACE,
            $($arg)+
        )
    );
}

#[macro_export]
macro_rules! debug {
    // Target.
    (target: $target:expr, { $($field:tt)* }, $($arg:tt)* ) => (
        $crate::__tracing::event!(target: $target, $crate::__tracing::Level::DEBUG, { $($field)* }, $($arg)*)
    );
    (target: $target:expr, $($k:ident).+ $($field:tt)* ) => (
        $crate::__tracing::event!(target: $target, $crate::__tracing::Level::DEBUG, { $($k).+ $($field)* })
    );
    (target: $target:expr, ?$($k:ident).+ $($field:tt)* ) => (
        $crate::__tracing::event!(target: $target, $crate::__tracing::Level::DEBUG, { ?$($k).+ $($field)* })
    );
    (target: $target:expr, %$($k:ident).+ $($field:tt)* ) => (
        $crate::__tracing::event!(target: $target, $crate::__tracing::Level::DEBUG, { %$($k).+ $($field)* })
    );
    (target: $target:expr, $($arg:tt)+ ) => (
        $crate::__tracing::event!(target: $target, $crate::__tracing::Level::DEBUG, {}, $($arg)+)
    );

    // ...
    ({ $($field:tt)+ }, $($arg:tt)+ ) => (
        $crate::__tracing::event!(
            target: $crate::transformed_module_path!(),
            $crate::__tracing::Level::DEBUG,
            { $($field)+ },
            $($arg)+
        )
    );
    ($($k:ident).+ = $($field:tt)*) => (
        $crate::__tracing::event!(
            target: $crate::transformed_module_path!(),
            $crate::__tracing::Level::DEBUG,
            { $($k).+ = $($field)*}
        )
    );
    (?$($k:ident).+ = $($field:tt)*) => (
        $crate::__tracing::event!(
            target: $crate::transformed_module_path!(),
            $crate::__tracing::Level::DEBUG,
            { ?$($k).+ = $($field)*}
        )
    );
    (%$($k:ident).+ = $($field:tt)*) => (
        $crate::__tracing::event!(
            target: $crate::transformed_module_path!(),
            $crate::__tracing::Level::DEBUG,
            { %$($k).+ = $($field)*}
        )
    );
    ($($k:ident).+, $($field:tt)*) => (
        $crate::__tracing::event!(
            target: $crate::transformed_module_path!(),
            $crate::__tracing::Level::DEBUG,
            { $($k).+, $($field)*}
        )
    );
    (?$($k:ident).+, $($field:tt)*) => (
        $crate::__tracing::event!(
            target: $crate::transformed_module_path!(),
            $crate::__tracing::Level::DEBUG,
            { ?$($k).+, $($field)*}
        )
    );
    (%$($k:ident).+, $($field:tt)*) => (
        $crate::__tracing::event!(
            target: $crate::transformed_module_path!(),
            $crate::__tracing::Level::DEBUG,
            { %$($k).+, $($field)*}
        )
    );
    (?$($k:ident).+) => (
        $crate::__tracing::event!(
            target: $crate::transformed_module_path!(),
            $crate::__tracing::Level::DEBUG,
            { ?$($k).+ }
        )
    );
    (%$($k:ident).+) => (
        $crate::__tracing::event!(
            target: $crate::transformed_module_path!(),
            $crate::__tracing::Level::DEBUG,
            { %$($k).+ }
        )
    );
    ($($k:ident).+) => (
        $crate::__tracing::event!(
            target: $crate::transformed_module_path!(),
            $crate::__tracing::Level::DEBUG,
            { $($k).+ }
        )
    );
    ($($arg:tt)+) => (
        $crate::__tracing::event!(
            target: $crate::transformed_module_path!(),
            $crate::__tracing::Level::DEBUG,
            $($arg)+
        )
    );
}

#[macro_export]
macro_rules! info {
    // Target.
    (target: $target:expr, { $($field:tt)* }, $($arg:tt)* ) => (
        $crate::__tracing::event!(target: $target, $crate::__tracing::Level::INFO, { $($field)* }, $($arg)*)
    );
    (target: $target:expr, $($k:ident).+ $($field:tt)* ) => (
        $crate::__tracing::event!(target: $target, $crate::__tracing::Level::INFO, { $($k).+ $($field)* })
    );
    (target: $target:expr, ?$($k:ident).+ $($field:tt)* ) => (
        $crate::__tracing::event!(target: $target, $crate::__tracing::Level::INFO, { ?$($k).+ $($field)* })
    );
    (target: $target:expr, %$($k:ident).+ $($field:tt)* ) => (
        $crate::__tracing::event!(target: $target, $crate::__tracing::Level::INFO, { %$($k).+ $($field)* })
    );
    (target: $target:expr, $($arg:tt)+ ) => (
        $crate::__tracing::event!(target: $target, $crate::__tracing::Level::INFO, {}, $($arg)+)
    );

    // ...
    ({ $($field:tt)+ }, $($arg:tt)+ ) => (
        $crate::__tracing::event!(
            target: $crate::transformed_module_path!(),
            $crate::__tracing::Level::INFO,
            { $($field)+ },
            $($arg)+
        )
    );
    ($($k:ident).+ = $($field:tt)*) => (
        $crate::__tracing::event!(
            target: $crate::transformed_module_path!(),
            $crate::__tracing::Level::INFO,
            { $($k).+ = $($field)*}
        )
    );
    (?$($k:ident).+ = $($field:tt)*) => (
        $crate::__tracing::event!(
            target: $crate::transformed_module_path!(),
            $crate::__tracing::Level::INFO,
            { ?$($k).+ = $($field)*}
        )
    );
    (%$($k:ident).+ = $($field:tt)*) => (
        $crate::__tracing::event!(
            target: $crate::transformed_module_path!(),
            $crate::__tracing::Level::INFO,
            { %$($k).+ = $($field)*}
        )
    );
    ($($k:ident).+, $($field:tt)*) => (
        $crate::__tracing::event!(
            target: $crate::transformed_module_path!(),
            $crate::__tracing::Level::INFO,
            { $($k).+, $($field)*}
        )
    );
    (?$($k:ident).+, $($field:tt)*) => (
        $crate::__tracing::event!(
            target: $crate::transformed_module_path!(),
            $crate::__tracing::Level::INFO,
            { ?$($k).+, $($field)*}
        )
    );
    (%$($k:ident).+, $($field:tt)*) => (
        $crate::__tracing::event!(
            target: $crate::transformed_module_path!(),
            $crate::__tracing::Level::INFO,
            { %$($k).+, $($field)*}
        )
    );
    (?$($k:ident).+) => (
        $crate::__tracing::event!(
            target: $crate::transformed_module_path!(),
            $crate::__tracing::Level::INFO,
            { ?$($k).+ }
        )
    );
    (%$($k:ident).+) => (
        $crate::__tracing::event!(
            target: $crate::transformed_module_path!(),
            $crate::__tracing::Level::INFO,
            { %$($k).+ }
        )
    );
    ($($k:ident).+) => (
        $crate::__tracing::event!(
            target: $crate::transformed_module_path!(),
            $crate::__tracing::Level::INFO,
            { $($k).+ }
        )
    );
    ($($arg:tt)+) => (
        $crate::__tracing::event!(
            target: $crate::transformed_module_path!(),
            $crate::__tracing::Level::INFO,
            $($arg)+
        )
    );
}

#[macro_export]
macro_rules! warn {
    // Target.
    (target: $target:expr, { $($field:tt)* }, $($arg:tt)* ) => (
        $crate::__tracing::event!(target: $target, $crate::__tracing::Level::WARN, { $($field)* }, $($arg)*)
    );
    (target: $target:expr, $($k:ident).+ $($field:tt)* ) => (
        $crate::__tracing::event!(target: $target, $crate::__tracing::Level::WARN, { $($k).+ $($field)* })
    );
    (target: $target:expr, ?$($k:ident).+ $($field:tt)* ) => (
        $crate::__tracing::event!(target: $target, $crate::__tracing::Level::WARN, { ?$($k).+ $($field)* })
    );
    (target: $target:expr, %$($k:ident).+ $($field:tt)* ) => (
        $crate::__tracing::event!(target: $target, $crate::__tracing::Level::WARN, { %$($k).+ $($field)* })
    );
    (target: $target:expr, $($arg:tt)+ ) => (
        $crate::__tracing::event!(target: $target, $crate::__tracing::Level::WARN, {}, $($arg)+)
    );

    // ...
    ({ $($field:tt)+ }, $($arg:tt)+ ) => (
        $crate::__tracing::event!(
            target: $crate::transformed_module_path!(),
            $crate::__tracing::Level::WARN,
            { $($field)+ },
            $($arg)+
        )
    );
    ($($k:ident).+ = $($field:tt)*) => (
        $crate::__tracing::event!(
            target: $crate::transformed_module_path!(),
            $crate::__tracing::Level::WARN,
            { $($k).+ = $($field)*}
        )
    );
    (?$($k:ident).+ = $($field:tt)*) => (
        $crate::__tracing::event!(
            target: $crate::transformed_module_path!(),
            $crate::__tracing::Level::WARN,
            { ?$($k).+ = $($field)*}
        )
    );
    (%$($k:ident).+ = $($field:tt)*) => (
        $crate::__tracing::event!(
            target: $crate::transformed_module_path!(),
            $crate::__tracing::Level::WARN,
            { %$($k).+ = $($field)*}
        )
    );
    ($($k:ident).+, $($field:tt)*) => (
        $crate::__tracing::event!(
            target: $crate::transformed_module_path!(),
            $crate::__tracing::Level::WARN,
            { $($k).+, $($field)*}
        )
    );
    (?$($k:ident).+, $($field:tt)*) => (
        $crate::__tracing::event!(
            target: $crate::transformed_module_path!(),
            $crate::__tracing::Level::WARN,
            { ?$($k).+, $($field)*}
        )
    );
    (%$($k:ident).+, $($field:tt)*) => (
        $crate::__tracing::event!(
            target: $crate::transformed_module_path!(),
            $crate::__tracing::Level::WARN,
            { %$($k).+, $($field)*}
        )
    );
    (?$($k:ident).+) => (
        $crate::__tracing::event!(
            target: $crate::transformed_module_path!(),
            $crate::__tracing::Level::WARN,
            { ?$($k).+ }
        )
    );
    (%$($k:ident).+) => (
        $crate::__tracing::event!(
            target: $crate::transformed_module_path!(),
            $crate::__tracing::Level::WARN,
            { %$($k).+ }
        )
    );
    ($($k:ident).+) => (
        $crate::__tracing::event!(
            target: $crate::transformed_module_path!(),
            $crate::__tracing::Level::WARN,
            { $($k).+ }
        )
    );
    ($($arg:tt)+) => (
        $crate::__tracing::event!(
            target: $crate::transformed_module_path!(),
            $crate::__tracing::Level::WARN,
            $($arg)+
        )
    );
}

#[macro_export]
macro_rules! error {
    // Target.
    (target: $target:expr, { $($field:tt)* }, $($arg:tt)* ) => (
        $crate::__tracing::event!(target: $target, $crate::__tracing::Level::ERROR, { $($field)* }, $($arg)*)
    );
    (target: $target:expr, $($k:ident).+ $($field:tt)* ) => (
        $crate::__tracing::event!(target: $target, $crate::__tracing::Level::ERROR, { $($k).+ $($field)* })
    );
    (target: $target:expr, ?$($k:ident).+ $($field:tt)* ) => (
        $crate::__tracing::event!(target: $target, $crate::__tracing::Level::ERROR, { ?$($k).+ $($field)* })
    );
    (target: $target:expr, %$($k:ident).+ $($field:tt)* ) => (
        $crate::__tracing::event!(target: $target, $crate::__tracing::Level::ERROR, { %$($k).+ $($field)* })
    );
    (target: $target:expr, $($arg:tt)+ ) => (
        $crate::__tracing::event!(target: $target, $crate::__tracing::Level::ERROR, {}, $($arg)+)
    );

    // ...
    ({ $($field:tt)+ }, $($arg:tt)+ ) => (
        $crate::__tracing::event!(
            target: $crate::transformed_module_path!(),
            $crate::__tracing::Level::ERROR,
            { $($field)+ },
            $($arg)+
        )
    );
    ($($k:ident).+ = $($field:tt)*) => (
        $crate::__tracing::event!(
            target: $crate::transformed_module_path!(),
            $crate::__tracing::Level::ERROR,
            { $($k).+ = $($field)*}
        )
    );
    (?$($k:ident).+ = $($field:tt)*) => (
        $crate::__tracing::event!(
            target: $crate::transformed_module_path!(),
            $crate::__tracing::Level::ERROR,
            { ?$($k).+ = $($field)*}
        )
    );
    (%$($k:ident).+ = $($field:tt)*) => (
        $crate::__tracing::event!(
            target: $crate::transformed_module_path!(),
            $crate::__tracing::Level::ERROR,
            { %$($k).+ = $($field)*}
        )
    );
    ($($k:ident).+, $($field:tt)*) => (
        $crate::__tracing::event!(
            target: $crate::transformed_module_path!(),
            $crate::__tracing::Level::ERROR,
            { $($k).+, $($field)*}
        )
    );
    (?$($k:ident).+, $($field:tt)*) => (
        $crate::__tracing::event!(
            target: $crate::transformed_module_path!(),
            $crate::__tracing::Level::ERROR,
            { ?$($k).+, $($field)*}
        )
    );
    (%$($k:ident).+, $($field:tt)*) => (
        $crate::__tracing::event!(
            target: $crate::transformed_module_path!(),
            $crate::__tracing::Level::ERROR,
            { %$($k).+, $($field)*}
        )
    );
    (?$($k:ident).+) => (
        $crate::__tracing::event!(
            target: $crate::transformed_module_path!(),
            $crate::__tracing::Level::ERROR,
            { ?$($k).+ }
        )
    );
    (%$($k:ident).+) => (
        $crate::__tracing::event!(
            target: $crate::transformed_module_path!(),
            $crate::__tracing::Level::ERROR,
            { %$($k).+ }
        )
    );
    ($($k:ident).+) => (
        $crate::__tracing::event!(
            target: $crate::transformed_module_path!(),
            $crate::__tracing::Level::ERROR,
            { $($k).+ }
        )
    );
    ($($arg:tt)+) => (
        $crate::__tracing::event!(
            target: $crate::transformed_module_path!(),
            $crate::__tracing::Level::ERROR,
            $($arg)+
        )
    );
}

#[macro_export]
macro_rules! transformed_module_path {
    () => {
        $crate::__string_literal_replace!(module_path!() ("zksync_os_" -> "zk::"))
    };
}

//! One module per `cargo xtask` subcommand (plus `ci`, which orchestrates the others).

pub(crate) mod ci;
pub(crate) mod dependency_hygiene;
pub(crate) mod review_variance;
pub(crate) mod schema;
pub(crate) mod test;
pub(crate) mod workspace;

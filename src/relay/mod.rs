//! Relays (DESIGN 5.1). This milestone has the shared HTTP policy, the text rules and Discord REST
//! posting for job destinations; intake, the pipeline and the other relays come later.

pub(crate) mod discord;
pub(crate) mod http;
pub(crate) mod text;

//! Why a network answer was not read: the network wants a login first,
//! does not reach the internet, or the server did not answer.

/// Why the server gave no answer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Unanswered {
    /// The network joined wants a login in a browser first.
    Login,
    /// The network joined does not reach the internet: why.
    Offline(String),
    /// The server could not be asked, or its answer not read: why.
    Failed(String),
}

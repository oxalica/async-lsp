//! Provides the `CanHandle` trait for services to indicate their ability to handle
//! specific message types. Useful for composing multiple Services/LspServices and
//! routing messages.
//!

/// Indicates whether a service can handle a specific message type.
pub trait CanHandle<Message> {
    /// Returns `true` if the service can handle the given message.
    fn can_handle(&self, e: &Message) -> bool;
}

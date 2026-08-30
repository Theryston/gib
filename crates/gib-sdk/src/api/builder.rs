use super::client::Client;
use super::error::{SdkError, SdkResult};
use super::event::EventDispatcher;

/// Default number of queued events retained per registered consumer.
pub const DEFAULT_EVENT_BUFFER_CAPACITY: usize = 64;

/// Builder for a validated [`Client`].
///
/// The builder contains only SDK policy. It does not inspect the environment,
/// open a repository, select a storage backend, or read credentials. A zero
/// event capacity is rejected by [`ClientBuilder::build`] because it would
/// make even critical lifecycle delivery impossible.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ClientBuilder {
    event_buffer_capacity: usize,
}

impl ClientBuilder {
    /// Creates a builder with the SDK defaults.
    pub const fn new() -> Self {
        Self {
            event_buffer_capacity: DEFAULT_EVENT_BUFFER_CAPACITY,
        }
    }

    /// Sets the bounded event queue capacity for each consumer.
    ///
    /// The value is validated when [`Self::build`] is called. A value of zero
    /// returns [`SdkError::InvalidConfiguration`].
    pub const fn event_buffer_capacity(mut self, capacity: usize) -> Self {
        self.event_buffer_capacity = capacity;
        self
    }

    /// Alias for [`Self::event_buffer_capacity`] using the shorter policy name.
    pub const fn event_capacity(self, capacity: usize) -> Self {
        self.event_buffer_capacity(capacity)
    }

    /// Returns the configured per-consumer queue capacity before validation.
    pub const fn configured_event_buffer_capacity(&self) -> usize {
        self.event_buffer_capacity
    }

    /// Validates the builder and creates an independent SDK client.
    pub fn build(self) -> SdkResult<Client> {
        if self.event_buffer_capacity == 0 {
            return Err(SdkError::InvalidConfiguration {
                field: "event_buffer_capacity",
                reason: "must be greater than zero",
            });
        }

        Ok(Client::from_dispatcher(
            EventDispatcher::from_valid_capacity(self.event_buffer_capacity),
        ))
    }
}

impl Default for ClientBuilder {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zero_event_capacity_is_rejected() {
        let result = ClientBuilder::new().event_buffer_capacity(0).build();
        assert!(matches!(
            result,
            Err(SdkError::InvalidConfiguration {
                field: "event_buffer_capacity",
                ..
            })
        ));
    }
}

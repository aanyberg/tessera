//! A minimal first-party plugin.
//!
//! Exists to exercise the built-in loader end to end: it counts sessions and
//! shows the count in the status bar. Also the template for new first-party
//! plugins such as the planned agent-usage tracker.

#![forbid(unsafe_code)]

use std::sync::Arc;

use tessera_plugin_api::{Capability, Event, Host, Manifest, Plugin, Result, StatusItem};

/// Session counter plugin.
#[derive(Default)]
pub struct Hello {
    host: Option<Arc<dyn Host>>,
    sessions: u32,
}

impl Hello {
    fn publish(&self) -> Result<()> {
        if let Some(host) = &self.host {
            host.set_status_items(vec![StatusItem {
                id: "sessions".into(),
                text: format!("{} sessions", self.sessions),
                tooltip: None,
            }])?;
        }
        Ok(())
    }
}

impl Plugin for Hello {
    fn manifest(&self) -> Manifest {
        Manifest {
            id: "dev.tessera.hello".into(),
            name: "Hello".into(),
            version: env!("CARGO_PKG_VERSION").into(),
            capabilities: vec![Capability::Sessions, Capability::StatusBar],
        }
    }

    fn activate(&mut self, host: Arc<dyn Host>) -> Result<()> {
        self.host = Some(host);
        self.publish()
    }

    fn on_event(&mut self, event: &Event) -> Result<()> {
        match event {
            Event::SessionCreated { .. } => self.sessions += 1,
            Event::SessionClosed { .. } => self.sessions = self.sessions.saturating_sub(1),
            _ => return Ok(()),
        }
        self.publish()
    }
}

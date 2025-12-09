//! Command routing for Signal admin messages.

use crate::message_handler::MessageHandler;
use std::fmt::Write;

/// How a command prefix should be handled.
pub enum Handling {
    /// Show the help/routing table.
    Help,
    /// Parse as a gateway command (e.g., /log, /query, /alerts).
    GatewayCommand,
    /// Send to Claude AI.
    Claude,
    /// Delegate to a custom message handler.
    Custom(Box<dyn MessageHandler>),
}

/// Routes incoming messages to appropriate handlers based on prefix matching.
///
/// The router contains an ordered list of (prefix, handling) pairs. When routing,
/// it finds the first prefix that matches the start of the message.
#[derive(Default)]
pub struct CommandRouter {
    routes: Vec<(String, Handling)>,
}


impl CommandRouter {
    /// Create a builder for constructing a CommandRouter.
    pub fn builder() -> CommandRouterBuilder {
        CommandRouterBuilder::default()
    }

    /// Route a message to the appropriate handler.
    ///
    /// Returns the handling and the message with the prefix stripped for the first
    /// prefix that matches the start of the message, or None if no prefix matches.
    pub fn route<'a>(&self, message: &'a str) -> Option<(&'a str, &Handling)> {
        self.routes.iter().find_map(|(prefix, handling)| {
            message
                .strip_prefix(prefix.as_str())
                .map(|stripped| (stripped, handling))
        })
    }

    /// Format the routing table as a help string.
    pub fn help(&self) -> String {
        let mut text = String::from("Command routing:\n");
        for (prefix, handling) in &self.routes {
            let prefix_display = if prefix.is_empty() {
                "(default)"
            } else {
                prefix
            };
            let handling_display = match handling {
                Handling::Help => "Show this help",
                Handling::GatewayCommand => "Gateway commands (/log, /query, /alerts, etc.)",
                Handling::Claude => "Claude AI",
                Handling::Custom(_) => "Custom handler",
            };
            writeln!(&mut text, "  {prefix_display:12} → {handling_display}").unwrap();
        }
        text
    }
}

/// Builder for constructing a CommandRouter.
#[derive(Default)]
pub struct CommandRouterBuilder {
    routes: Vec<(String, Handling)>,
}

impl CommandRouterBuilder {
    /// Add a route mapping a prefix to a handling.
    ///
    /// Routes are matched in the order they are added.
    pub fn route(mut self, prefix: impl Into<String>, handling: Handling) -> Self {
        self.routes.push((prefix.into(), handling));
        self
    }

    /// Build the CommandRouter.
    ///
    /// # Panics
    ///
    /// Panics if any route is completely inaccessible because an earlier prefix
    /// is a prefix of it.
    pub fn build(self) -> CommandRouter {
        // Check for inaccessible routes
        for (i, (prefix_i, _)) in self.routes.iter().enumerate() {
            for (j, (prefix_j, _)) in self.routes.iter().enumerate().skip(i + 1) {
                if prefix_j.starts_with(prefix_i) {
                    panic!(
                        "Route '{}' at index {} is inaccessible because '{}' at index {} is a prefix of it",
                        prefix_j, j, prefix_i, i
                    );
                }
            }
        }

        CommandRouter {
            routes: self.routes,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_route_exact_match() {
        let router = CommandRouter::builder()
            .route("/help", Handling::Help)
            .route("/", Handling::GatewayCommand)
            .build();

        assert!(matches!(router.route("/help"), Some(("", Handling::Help))));
        assert!(matches!(
            router.route("/log"),
            Some(("log", Handling::GatewayCommand))
        ));
    }

    #[test]
    fn test_route_prefix_match() {
        let router = CommandRouter::builder()
            .route("#", Handling::Help)
            .route("", Handling::Claude)
            .build();

        assert!(matches!(
            router.route("#anything"),
            Some(("anything", Handling::Help))
        ));
        assert!(matches!(
            router.route("hello"),
            Some(("hello", Handling::Claude))
        ));
    }

    #[test]
    fn test_route_no_match() {
        let router = CommandRouter::builder()
            .route("/", Handling::GatewayCommand)
            .build();

        assert!(router.route("hello").is_none());
    }

    #[test]
    fn test_route_order_matters() {
        let router = CommandRouter::builder()
            .route("--help", Handling::Help)
            .route("-h", Handling::Help)
            .route("/", Handling::GatewayCommand)
            .route("", Handling::Claude)
            .build();

        assert!(matches!(router.route("--help"), Some(("", Handling::Help))));
        assert!(matches!(router.route("-h"), Some(("", Handling::Help))));
        assert!(matches!(
            router.route("/log"),
            Some(("log", Handling::GatewayCommand))
        ));
        assert!(matches!(
            router.route("hello"),
            Some(("hello", Handling::Claude))
        ));
    }

    #[test]
    #[should_panic(expected = "inaccessible")]
    fn test_inaccessible_route_panics() {
        CommandRouter::builder()
            .route("/", Handling::GatewayCommand)
            .route("/help", Handling::Help) // This is inaccessible because "/" comes first
            .build();
    }

    #[test]
    fn test_empty_router() {
        let router = CommandRouter::default();
        assert!(router.route("anything").is_none());
    }
}

use crate::core::event::ToolDisplay;
use std::sync::Arc;

pub trait ToolDisplayFormatter: Send + Sync {
    fn format_tool_display(&self, name: &str, args: &str) -> anyhow::Result<Option<ToolDisplay>>;
}

#[derive(Default, Clone)]
pub struct ToolDisplayRegistry {
    formatters: Vec<Arc<dyn ToolDisplayFormatter>>,
}

impl ToolDisplayFormatter for ToolDisplayRegistry {
    fn format_tool_display(&self, name: &str, args: &str) -> anyhow::Result<Option<ToolDisplay>> {
        for formatter in &self.formatters {
            match formatter.format_tool_display(name, args) {
                Ok(Some(display)) => return Ok(Some(display)),
                Ok(None) => {}
                Err(error) => tracing::warn!(tool = name, "tool display formatter failed: {error}"),
            }
        }
        Ok(None)
    }
}

impl ToolDisplayRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_formatter(mut self, formatter: Arc<dyn ToolDisplayFormatter>) -> Self {
        self.push(formatter);
        self
    }

    pub fn push(&mut self, formatter: Arc<dyn ToolDisplayFormatter>) {
        self.formatters.push(formatter);
    }

    pub(crate) fn remove_formatter(&mut self, removed: &Arc<dyn ToolDisplayFormatter>) {
        self.formatters
            .retain(|formatter| !Arc::ptr_eq(formatter, removed));
    }

    pub(crate) fn replace_formatter(
        &mut self,
        previous: &Arc<dyn ToolDisplayFormatter>,
        replacement: Arc<dyn ToolDisplayFormatter>,
    ) {
        for formatter in &mut self.formatters {
            if Arc::ptr_eq(formatter, previous) {
                *formatter = replacement.clone();
            }
        }
    }

    pub fn format_tool_display(
        &self,
        name: &str,
        args: &str,
    ) -> anyhow::Result<Option<ToolDisplay>> {
        ToolDisplayFormatter::format_tool_display(self, name, args)
    }
}

impl<F> ToolDisplayFormatter for F
where
    F: Fn(&str, &str) -> anyhow::Result<Option<ToolDisplay>> + Send + Sync,
{
    fn format_tool_display(&self, name: &str, args: &str) -> anyhow::Result<Option<ToolDisplay>> {
        self(name, args)
    }
}

impl<T> ToolDisplayFormatter for Arc<T>
where
    T: ToolDisplayFormatter + ?Sized,
{
    fn format_tool_display(&self, name: &str, args: &str) -> anyhow::Result<Option<ToolDisplay>> {
        self.as_ref().format_tool_display(name, args)
    }
}

#[cfg(test)]
mod display_tests {
    use super::*;

    #[test]
    fn continues_after_formatter_failure() {
        let registry = ToolDisplayRegistry::new()
            .with_formatter(Arc::new(|_: &str, _: &str| anyhow::bail!("broken")))
            .with_formatter(Arc::new(|_: &str, _: &str| {
                Ok(Some(ToolDisplay::title("fallback")))
            }));

        let display = registry.format_tool_display("test", "{}").unwrap();

        assert_eq!(
            display.and_then(|display| display.title).as_deref(),
            Some("fallback")
        );
    }
}

use crate::{InputContext, KeyInput};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeyBinding<A> {
    pub context: InputContext,
    pub key: KeyInput,
    pub action: A,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeyBindingResolver<A> {
    bindings: Vec<KeyBinding<A>>,
}

impl<A> Default for KeyBindingResolver<A> {
    fn default() -> Self {
        Self {
            bindings: Vec::new(),
        }
    }
}

impl<A> KeyBindingResolver<A> {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn bind(mut self, context: InputContext, key: KeyInput, action: A) -> Self {
        self.bindings.push(KeyBinding {
            context,
            key,
            action,
        });
        self
    }

    pub fn bindings(&self) -> &[KeyBinding<A>] {
        &self.bindings
    }
}

impl<A: Clone> KeyBindingResolver<A> {
    pub fn resolve(&self, active_context: InputContext, key: &KeyInput) -> Option<A> {
        self.bindings
            .iter()
            .rev()
            .find(|binding| binding.context == active_context && &binding.key == key)
            .or_else(|| {
                self.bindings
                    .iter()
                    .rev()
                    .find(|binding| binding.context == InputContext::Global && &binding.key == key)
            })
            .map(|binding| binding.action.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug, Clone, PartialEq, Eq)]
    enum Action {
        Quit,
        Submit,
        Override,
    }

    #[test]
    fn context_bindings_override_and_fall_back_to_global() {
        let resolver = KeyBindingResolver::new()
            .bind(InputContext::Global, KeyInput::Enter, Action::Submit)
            .bind(InputContext::Picker, KeyInput::Enter, Action::Override)
            .bind(InputContext::Global, KeyInput::Ctrl('c'), Action::Quit);

        assert_eq!(
            resolver.resolve(InputContext::Picker, &KeyInput::Enter),
            Some(Action::Override)
        );
        assert_eq!(
            resolver.resolve(InputContext::Editor, &KeyInput::Ctrl('c')),
            Some(Action::Quit)
        );
    }
}

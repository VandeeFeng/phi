#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct FocusId(u64);

impl FocusId {
    pub const ROOT: Self = Self(0);

    pub fn new(value: u64) -> Self {
        Self(value)
    }

    pub fn get(self) -> u64 {
        self.0
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum InputContext {
    Global,
    Editor,
    Transcript,
    Picker,
    Confirm,
    Overlay,
    Dialog,
    Custom(&'static str),
}

impl InputContext {
    pub const fn custom(name: &'static str) -> Self {
        Self::Custom(name)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct FocusHost {
    active: FocusId,
    stack: Vec<FocusId>,
    next_id: u64,
}

impl Default for FocusHost {
    fn default() -> Self {
        Self {
            active: FocusId::ROOT,
            stack: Vec::new(),
            next_id: 1,
        }
    }
}

impl FocusHost {
    #[cfg(test)]
    pub fn new() -> Self {
        Self::default()
    }

    pub fn allocate(&mut self) -> FocusId {
        let id = FocusId::new(self.next_id);
        self.next_id += 1;
        id
    }

    pub fn active(&self) -> FocusId {
        self.active
    }

    pub fn focus(&mut self, id: FocusId) {
        if self.active != id {
            self.stack.retain(|entry| *entry != id);
            self.stack.push(self.active);
            self.active = id;
        }
    }

    pub fn restore(&mut self) -> FocusId {
        self.active = self.stack.pop().unwrap_or(FocusId::ROOT);
        self.active
    }

    pub fn remove(&mut self, id: FocusId) -> FocusId {
        self.stack.retain(|entry| *entry != id);
        if self.active == id {
            self.active = self.stack.pop().unwrap_or(FocusId::ROOT);
        }
        self.active
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn revisiting_focus_does_not_create_a_restore_cycle() {
        let mut host = FocusHost::new();
        let first = host.allocate();
        let second = host.allocate();

        host.focus(first);
        host.focus(second);
        host.focus(first);

        assert_eq!(host.restore(), second);
        assert_eq!(host.restore(), FocusId::ROOT);
    }
}

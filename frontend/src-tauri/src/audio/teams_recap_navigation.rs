//! Bounded recovery when Teams discards a recap deep link during sign-in.

#[derive(Debug, PartialEq)]
pub enum Action {
    Wait,
    RetryLink,
    ReportWrongPage,
}

#[derive(Default)]
pub struct Navigation {
    chat_since_ms: Option<u64>,
    retried: bool,
}

impl Navigation {
    pub fn observe(&mut self, status: &str, now_ms: u64) -> Action {
        if status != "signed_in_chat" {
            self.chat_since_ms = None;
            return Action::Wait;
        }
        let since = *self.chat_since_ms.get_or_insert(now_ms);
        let elapsed = now_ms.saturating_sub(since);
        if !self.retried && elapsed >= 8_000 {
            self.retried = true;
            self.chat_since_ms = None;
            Action::RetryLink
        } else if self.retried && elapsed >= 30_000 {
            Action::ReportWrongPage
        } else {
            Action::Wait
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retries_after_sign_in_then_reports_persistent_wrong_page() {
        let mut nav = Navigation::default();
        assert_eq!(nav.observe("signed_in_chat", 0), Action::Wait);
        assert_eq!(nav.observe("signed_in_chat", 7_999), Action::Wait);
        assert_eq!(nav.observe("signed_in_chat", 8_000), Action::RetryLink);
        assert_eq!(nav.observe("waiting", 9_000), Action::Wait);
        assert_eq!(nav.observe("signed_in_chat", 10_000), Action::Wait);
        assert_eq!(nav.observe("signed_in_chat", 39_999), Action::Wait);
        assert_eq!(
            nav.observe("signed_in_chat", 40_000),
            Action::ReportWrongPage
        );
    }

    #[test]
    fn never_interrupts_sign_in_or_a_loading_recap() {
        let mut nav = Navigation::default();
        for status in ["waiting", "launcher", "opening_web", "ready"] {
            assert_eq!(nav.observe(status, 300_000), Action::Wait);
        }
        assert_eq!(nav.observe("signed_in_chat", 310_000), Action::Wait);
        assert_eq!(nav.observe("waiting", 315_000), Action::Wait);
        assert_eq!(nav.observe("signed_in_chat", 320_000), Action::Wait);
    }
}

//! Keep one inference state until model unload; never retry a failed native
//! state automatically. The caller holds an exclusive lock for run + extraction.
pub(super) struct StateCache<S> {
    state: Option<S>,
    failure: Option<String>,
}
impl<S> Default for StateCache<S> {
    fn default() -> Self {
        Self {
            state: None,
            failure: None,
        }
    }
}
impl<S> StateCache<S> {
    pub fn reset(&mut self) {
        self.state = None;
        self.failure = None;
    }
    pub fn run<E: std::fmt::Display, R>(
        &mut self,
        create: impl FnOnce() -> Result<S, E>,
        infer: impl FnOnce(&mut S) -> Result<R, E>,
    ) -> Result<&mut S, String> {
        if let Some(error) = &self.failure {
            return Err(error.clone());
        }
        if self.state.is_none() {
            match create() {
                Ok(state) => self.state = Some(state),
                Err(error) => {
                    let error = error.to_string();
                    self.failure = Some(error.clone());
                    return Err(error);
                }
            }
        }
        if let Err(error) = infer(self.state.as_mut().unwrap()) {
            let error = error.to_string();
            self.failure = Some(error.clone());
            // Keep the failed state quarantined until explicit model unload.
            // Do not re-enter potentially broken native state or retry on CPU.
            return Err(error);
        }
        Ok(self.state.as_mut().unwrap())
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn chunks_reuse_state_and_reset_releases_it() {
        let mut cache = StateCache::default();
        for _ in 0..4 {
            cache
                .run(
                    || Ok::<_, &str>(0),
                    |s| {
                        *s += 1;
                        Ok::<_, &str>(())
                    },
                )
                .unwrap();
        }
        assert_eq!(cache.state, Some(4));
        cache.reset();
        assert_eq!(
            *cache
                .run(|| Ok::<_, &str>(10), |_| Ok::<_, &str>(()))
                .unwrap(),
            10
        );
    }
    #[test]
    fn exception_quarantines_state_without_retry_or_fallback() {
        let mut cache = StateCache::default();
        assert_eq!(
            cache
                .run(|| Ok::<_, &str>(0), |_| Err::<(), _>("device lost"))
                .err()
                .unwrap(),
            "device lost"
        );
        assert_eq!(
            cache
                .run(
                    || -> Result<i32, &str> { panic!("must not allocate") },
                    |_| -> Result<(), &str> { panic!("must not retry") }
                )
                .err()
                .unwrap(),
            "device lost"
        );
        cache.reset();
        assert!(cache
            .run(|| Ok::<_, &str>(1), |_| Ok::<_, &str>(()))
            .is_ok());
    }
    #[test]
    fn allocation_failure_is_retained_until_reset() {
        let mut cache = StateCache::<i32>::default();
        assert!(cache
            .run(|| Err::<i32, _>("allocation failed"), |_| Ok::<_, &str>(()))
            .is_err());
        assert!(cache
            .run(
                || -> Result<i32, &str> { panic!("no automatic retry") },
                |_| Ok::<_, &str>(())
            )
            .is_err());
    }
}

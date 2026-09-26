use std::collections::VecDeque;

#[derive(Default)]
pub struct Recovery {
    since: Option<u64>,
    last_attempt: Option<u64>,
    progress: u64,
}
impl Recovery {
    pub fn due(&mut self, now: u64, stalled: bool, progress: u64) -> bool {
        if !stalled || self.progress != progress {
            self.since = None;
            self.progress = progress;
            return false;
        }
        let since = *self.since.get_or_insert(now);
        if now.saturating_sub(since) < 120
            || self
                .last_attempt
                .is_some_and(|t| now.saturating_sub(t) < 600)
        {
            return false;
        }
        self.last_attempt = Some(now);
        self.since = Some(now);
        true
    }
}

pub struct Window {
    started: u64,
    sampled: u64,
    bytes: u64,
    last_data: u64,
    samples: VecDeque<(u64, u64, u64)>,
}
#[derive(Clone, Copy)]
pub struct Quality {
    pub rank: u8,
    pub rate: u64,
    pub idle: u64,
    pub label: &'static str,
}
impl Window {
    pub fn new(now: u64, bytes: u64) -> Self {
        Self {
            started: now,
            sampled: now,
            bytes,
            last_data: now,
            samples: VecDeque::new(),
        }
    }
    pub fn observe(&mut self, now: u64, bytes: u64) -> Quality {
        let elapsed = now.saturating_sub(self.sampled);
        if bytes < self.bytes || elapsed > 30 {
            *self = Self::new(now, bytes);
        } else if elapsed >= 10 {
            let delta = bytes - self.bytes;
            if delta > 0 {
                self.last_data = now;
            }
            self.samples.push_back((now, delta, elapsed));
            self.bytes = bytes;
            self.sampled = now;
        }
        while self
            .samples
            .front()
            .is_some_and(|s| now.saturating_sub(s.0) >= 60)
        {
            self.samples.pop_front();
        }
        self.quality(now)
    }
    pub fn quality(&self, now: u64) -> Quality {
        let elapsed = self.samples.iter().map(|s| s.2).sum::<u64>();
        let rate = self.samples.iter().map(|s| s.1).sum::<u64>() / elapsed.max(1);
        let idle = now.saturating_sub(self.last_data);
        let stable = self.samples.len() >= 3
            && self.samples.iter().rev().take(3).all(|s| s.1 > 0)
            && idle < 20;
        let (rank, label) = if stable {
            (0, "持续收数")
        } else if idle < 30 && rate > 0 {
            (1, "间歇收数")
        } else if now.saturating_sub(self.started) < 60 {
            (2, "观察中")
        } else {
            (3, "暂无有效传输")
        };
        Quality {
            rank,
            rate,
            idle,
            label,
        }
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn recovery_requires_stall_and_respects_progress_and_cooldown() {
        let mut r = Recovery::default();
        assert!(!r.due(0, true, 0));
        assert!(!r.due(119, true, 0));
        assert!(r.due(120, true, 0));
        assert!(!r.due(300, true, 0));
        assert!(!r.due(720, false, 0));
        assert!(!r.due(730, true, 1));
        assert!(!r.due(740, true, 1));
        assert!(r.due(860, true, 1));
    }
    #[test]
    fn sustained_transfer_outranks_bursts_and_idle_peers() {
        let mut stable = Window::new(0, 0);
        for n in 1..=3 {
            stable.observe(n * 10, n * 100);
        }
        assert_eq!(stable.quality(30).rank, 0);
        let mut burst = Window::new(0, 0);
        burst.observe(10, 1000000);
        burst.observe(20, 1000000);
        burst.observe(30, 1000000);
        assert!(burst.quality(30).rank > stable.quality(30).rank);
        for n in 4..=9 {
            stable.observe(n * 10, 300);
        }
        assert_eq!(stable.quality(90).rank, 3);
        assert_eq!(stable.observe(100, 1).rank, 2); // A restarted counter starts a new observation.
        assert_eq!(Window::new(0, 0).observe(120, 500).rank, 2); // A gap is not sustained transfer.
    }
}

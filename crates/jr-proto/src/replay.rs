/// 每个方向 64 包重放窗。seq 从 1 开始，允许跳号，不允许复用。
#[derive(Debug, Default)]
pub struct ReplayWindow {
    highest: u32,
    /// bit 0 是 highest，bit i 是 highest-i。
    seen: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReplayError {
    Zero,
    Replay,
    TooOld,
}

impl ReplayWindow {
    pub fn check(&mut self, seq: u32) -> Result<(), ReplayError> {
        if seq == 0 {
            return Err(ReplayError::Zero);
        }
        if self.highest == 0 {
            self.highest = seq;
            self.seen = 1;
            return Ok(());
        }
        if seq > self.highest {
            let shift = seq - self.highest;
            if shift >= 64 {
                self.seen = 1;
            } else {
                self.seen = (self.seen << shift) | 1;
            }
            self.highest = seq;
            return Ok(());
        }
        let diff = self.highest - seq;
        if diff >= 64 {
            return Err(ReplayError::TooOld);
        }
        let bit = 1u64 << diff;
        if self.seen & bit != 0 {
            return Err(ReplayError::Replay);
        }
        self.seen |= bit;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn replay_and_old() {
        let mut w = ReplayWindow::default();
        assert_eq!(w.check(0), Err(ReplayError::Zero));
        w.check(5).unwrap();
        assert_eq!(w.check(5), Err(ReplayError::Replay));
        w.check(4).unwrap();
        assert_eq!(w.check(4), Err(ReplayError::Replay));
        w.check(100).unwrap();
        assert_eq!(w.check(10), Err(ReplayError::TooOld));
        w.check(99).unwrap();
        assert_eq!(w.check(99), Err(ReplayError::Replay));
    }
}

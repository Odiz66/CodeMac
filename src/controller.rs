//! Hold/release decision for the reel minigame.
//!
//! The bar behaves like a mass: holding LMB accelerates it to the right,
//! releasing lets it accelerate to the left. A PD law on the predicted error
//! gives a desired "push" in [0, 1] (the duty cycle), and a first-order
//! sigma-delta modulator turns that into a hold/release pattern whose average
//! equals the duty. Near zero error the bar is held still by alternating at the
//! neutral duty instead of swinging between full left and full right.

use crate::config::{ControlMode, ControllerCfg};

/// Predicted state, all in pixels / pixels per second along the track.
#[derive(Debug, Clone, Copy)]
pub struct CtlInput {
    pub fish_x: f32,
    pub fish_v: f32,
    pub bar_x: f32,
    pub bar_v: f32,
    pub bar_w: f32,
    pub track_w: f32,
}

#[derive(Debug, Clone, Copy, Default)]
pub struct CtlOutput {
    pub hold: bool,
    pub duty: f32,
    /// Fish minus bar position in track widths.
    pub error: f32,
    /// True when the edge guard overrode the PD output.
    pub guard: bool,
}

pub struct Controller {
    cfg: ControllerCfg,
    acc: f32,
}

impl Controller {
    pub fn new(cfg: &ControllerCfg) -> Controller {
        Controller {
            cfg: cfg.clone(),
            acc: 0.0,
        }
    }

    /// Output when there is nothing to track: hover at the neutral duty.
    pub fn idle(&mut self) -> CtlOutput {
        let duty = self.cfg.neutral_duty;
        CtlOutput {
            hold: self.modulate(duty),
            duty,
            error: 0.0,
            guard: false,
        }
    }

    pub fn update(&mut self, s: &CtlInput) -> CtlOutput {
        let c = &self.cfg;
        let tw = s.track_w.max(1.0);
        let offset = s.fish_x - s.bar_x;
        let e = offset / tw;
        let de = (s.fish_v - s.bar_v) / tw;
        let mut duty = match c.mode {
            ControlMode::Pd => (c.neutral_duty + c.kp * e + c.kd * de).clamp(0.0, 1.0),
            ControlMode::BangBang => {
                if c.kp * e + c.kd * de > 0.0 {
                    1.0
                } else {
                    0.0
                }
            }
        };

        // Edge guard: the fish is about to leave the bar and still moving away
        // from its centre -> push as hard as possible.
        let half = s.bar_w / 2.0;
        let inner = half * (1.0 - c.edge_guard.clamp(0.0, 1.0));
        let mut guard = false;
        if half > 0.0 {
            if offset > inner && de >= 0.0 {
                duty = 1.0;
                guard = true;
            } else if offset < -inner && de <= 0.0 {
                duty = 0.0;
                guard = true;
            }
        }

        CtlOutput {
            hold: self.modulate(duty),
            duty,
            error: e,
            guard,
        }
    }

    fn modulate(&mut self, duty: f32) -> bool {
        self.acc += duty;
        let hold = self.acc >= 0.5;
        if hold {
            self.acc -= 1.0;
        }
        self.acc = self.acc.clamp(-1.0, 1.0);
        hold
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn input(fish_x: f32, bar_x: f32) -> CtlInput {
        CtlInput {
            fish_x,
            fish_v: 0.0,
            bar_x,
            bar_v: 0.0,
            bar_w: 60.0,
            track_w: 1000.0,
        }
    }

    #[test]
    fn pushes_towards_the_fish() {
        let mut c = Controller::new(&ControllerCfg::default());
        assert!(c.update(&input(800.0, 300.0)).hold);
        assert!(!c.update(&input(100.0, 600.0)).hold);
    }

    #[test]
    fn neutral_duty_alternates() {
        let mut c = Controller::new(&ControllerCfg::default());
        let holds = (0..100)
            .filter(|_| c.update(&input(500.0, 500.0)).hold)
            .count();
        assert_eq!(holds, 50);
    }

    #[test]
    fn duty_is_reproduced_on_average() {
        let mut c = Controller::new(&ControllerCfg {
            kp: 10.0,
            kd: 0.0,
            ..Default::default()
        });
        // e = 0.02 -> duty = 0.5 + 0.2 = 0.7
        let holds = (0..1000)
            .filter(|_| c.update(&input(520.0, 500.0)).hold)
            .count();
        assert!((holds as i32 - 700).abs() <= 1, "{holds}");
    }

    #[test]
    fn edge_guard_saturates() {
        let mut c = Controller::new(&ControllerCfg {
            kp: 1.0,
            kd: 0.0,
            ..Default::default()
        });
        // Offset 25 of half-width 30 with guard 0.3 (inner 21) -> full hold every frame.
        for _ in 0..10 {
            let o = c.update(&input(525.0, 500.0));
            assert!(o.guard && o.hold);
        }
        // Moving back towards the centre -> guard does not fire.
        let o = c.update(&CtlInput {
            fish_v: -200.0,
            ..input(525.0, 500.0)
        });
        assert!(!o.guard);
    }
}

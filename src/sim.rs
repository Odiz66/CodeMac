//! Offline sanity check of the reel logic against a simulated minigame.
//!
//! The real Fisch physics are not public, so this is only a plausible model:
//! an accelerating bar, a fish that wanders to random targets, the game
//! running at `game_fps`, the screen (and therefore every capture) at `refresh_hz`,
//! plus capture and input latency. The macro side is the real `Reel` logic,
//! deciding hold/release once per captured frame like `run.rs` does.
//! (Playing the duty cycle out at 240 Hz between captures was tried here and
//! did not help: the limit is how old each captured frame is, not how finely the
//! hold is modulated.) It guards against regressions and was used to pick the
//! defaults; tune on the real game afterwards.

#[cfg(test)]
pub(crate) mod tests {
    use std::collections::VecDeque;

    use crate::config::{Config, ControlMode};
    use crate::detect::{Detection, Span};
    use crate::reel::Reel;

    struct Rng(u64);

    impl Rng {
        fn next(&mut self) -> f64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            (self.0 >> 11) as f64 / (1u64 << 53) as f64
        }
    }

    #[derive(Clone, Copy)]
    pub struct Physics {
        pub game_fps: f64,
        /// Monitor refresh rate: the screen, and so every capture, updates at this rate.
        pub refresh_hz: f64,
        /// Bar acceleration (px/s^2) while holding / releasing.
        pub accel: f64,
        pub max_speed: f64,
        /// Fish top speed and acceleration.
        pub fish_speed: f64,
        pub fish_accel: f64,
        /// Time from the screen refresh until the macro has the image (s).
        pub capture_age: f64,
        /// Time from a uinput event until the game can see it (s).
        pub input_delay: f64,
    }

    pub fn physics_240() -> Physics {
        Physics {
            game_fps: 240.0,
            refresh_hz: 60.0,
            accel: 3000.0,
            max_speed: 1500.0,
            fish_speed: 350.0,
            fish_accel: 1500.0,
            capture_age: 0.004,
            input_delay: 0.003,
        }
    }

    /// Returns the fraction of game frames in which the fish centre was inside the bar.
    pub fn simulate(cfg: &Config, phys: &Physics, seconds: f64, seed: u64) -> f64 {
        const W: f64 = 1000.0;
        let refresh = 1.0 / phys.refresh_hz;
        let dt = 1.0 / phys.game_fps;
        let mut rng = Rng(seed);
        let mut reel = Reel::new(cfg, W as f32);

        let (mut bar_x, mut bar_v) = (W / 2.0, 0.0);
        let (mut fish_x, mut fish_v) = (W / 2.0, 0.0);
        let mut target = W / 2.0;
        let mut next_target = 0.5;
        // (time, bar_x, bar_w, fish_x) of recent game frames.
        let mut shown: VecDeque<(f64, f64, f64, f64)> = VecDeque::new();
        // Input timeline: (time, held).
        let mut inputs: Vec<(f64, bool)> = vec![(f64::NEG_INFINITY, false)];
        let mut input_idx = 0;

        let mut next_frame = 0.0;
        let mut next_refresh = refresh;
        let (mut inside, mut total) = (0usize, 0usize);

        while next_frame < seconds {
            let receipt = next_refresh + phys.capture_age;
            let t = next_frame.min(receipt);
            if t == next_frame {
                // Game frame: physics with the input as the game sees it now.
                while input_idx + 1 < inputs.len()
                    && inputs[input_idx + 1].0 <= t - phys.input_delay
                {
                    input_idx += 1;
                }
                let hold = inputs[input_idx].1;
                // The bar shrinks from 90 px to 50 px over the reel, like the Ruinous Oath.
                let bar_w = 90.0 - 40.0 * (t / seconds);
                bar_v = (bar_v + if hold { phys.accel } else { -phys.accel } * dt)
                    .clamp(-phys.max_speed, phys.max_speed);
                bar_x += bar_v * dt;
                if bar_x < bar_w / 2.0 {
                    bar_x = bar_w / 2.0;
                    bar_v = 0.0;
                }
                if bar_x > W - bar_w / 2.0 {
                    bar_x = W - bar_w / 2.0;
                    bar_v = 0.0;
                }
                if t >= next_target {
                    target = 60.0 + rng.next() * (W - 120.0);
                    next_target = t + 0.4 + rng.next() * 1.2;
                }
                let want = ((target - fish_x) * 4.0).clamp(-phys.fish_speed, phys.fish_speed);
                fish_v += (want - fish_v).clamp(-phys.fish_accel * dt, phys.fish_accel * dt);
                fish_x += fish_v * dt;
                total += 1;
                if (fish_x - bar_x).abs() <= bar_w / 2.0 {
                    inside += 1;
                }
                shown.push_back((t, bar_x, bar_w, fish_x));
                while shown.len() > 64 {
                    shown.pop_front();
                }
                next_frame += dt;
            } else {
                // The screen showed the last game frame finished before the refresh.
                let Some(&(_, bx, bw, fx)) = shown.iter().rev().find(|f| f.0 <= next_refresh - dt)
                else {
                    next_refresh += refresh;
                    continue;
                };
                let fl = (fx - 4.0).round() as f32;
                let det = Detection {
                    bar: Some(Span {
                        left: (bx - bw / 2.0).round() as f32,
                        right: (bx + bw / 2.0).round() as f32,
                    }),
                    fish: vec![Span {
                        left: fl,
                        right: fl + 8.0,
                    }],
                    progress: None,
                };
                let step = reel.step(t, &det, false);
                inputs.push((t, step.hold));
                next_refresh += refresh;
            }
        }
        inside as f64 / total as f64
    }

    fn mean(cfg: &Config, phys: &Physics, seeds: std::ops::Range<u64>, secs: f64) -> f64 {
        let n = seeds.end - seeds.start;
        seeds
            .map(|s| simulate(cfg, phys, secs, s * 31 + 5))
            .sum::<f64>()
            / n as f64
    }

    #[test]
    fn default_tracks_a_wandering_fish_at_240_fps() {
        let cfg = Config::default();
        for seed in [1, 7, 42, 1234] {
            let r = simulate(&cfg, &physics_240(), 30.0, seed);
            println!("seed {seed}: inside {:.2}%", r * 100.0);
            assert!(r > 0.995, "seed {seed}: only {:.1}% inside", r * 100.0);
        }
    }

    #[test]
    fn high_refresh_monitor_tracks_better() {
        // Same fish and bar; only the monitor refresh rate (= capture rate) differs.
        let cfg = Config::default();
        let hard = Physics {
            accel: 6000.0,
            max_speed: 2000.0,
            fish_speed: 600.0,
            fish_accel: 5000.0,
            capture_age: 0.006,
            ..physics_240()
        };
        let fast = Physics {
            refresh_hz: 170.0,
            ..hard
        };
        let a = mean(&cfg, &fast, 0..6, 20.0);
        let b = mean(&cfg, &hard, 0..6, 20.0);
        println!("170 Hz {:.2}%, 60 Hz {:.2}%", a * 100.0, b * 100.0);
        assert!(a > b, "170 Hz {a} vs 60 Hz {b}");
        assert!(a > 0.98, "170 Hz only {:.1}%", a * 100.0);
    }

    #[test]
    fn copes_with_slow_capture_and_a_fast_fish() {
        let cfg = Config::default();
        let phys = Physics {
            accel: 6000.0,
            max_speed: 2000.0,
            fish_speed: 600.0,
            fish_accel: 5000.0,
            capture_age: 0.018,
            ..physics_240()
        };
        let r = mean(&cfg, &phys, 0..4, 30.0);
        println!("hard case: inside {:.2}%", r * 100.0);
        // Worst case for the defaults (tuned for fast capture); a larger
        // lookahead_ms recovers ~4 points here.
        assert!(r > 0.90, "only {:.1}% inside", r * 100.0);
    }

    #[test]
    fn faster_game_fps_helps() {
        // Same fish and bar; the 60 FPS game reads input up to a frame later.
        let cfg = Config::default();
        let hard = Physics {
            accel: 6000.0,
            fish_speed: 500.0,
            fish_accel: 3000.0,
            capture_age: 0.010,
            ..physics_240()
        };
        let slow = Physics {
            game_fps: 60.0,
            input_delay: 0.012,
            ..hard
        };
        let a = mean(&cfg, &hard, 0..6, 20.0);
        let b = mean(&cfg, &slow, 0..6, 20.0);
        println!("240 FPS {:.2}%, 60 FPS {:.2}%", a * 100.0, b * 100.0);
        assert!(a > b, "240 fps {a} vs 60 fps {b}");
    }

    #[test]
    fn works_with_a_60_fps_game() {
        let cfg = Config::default();
        let phys = Physics {
            game_fps: 60.0,
            input_delay: 0.017,
            capture_age: 0.008,
            ..physics_240()
        };
        let r = mean(&cfg, &phys, 0..4, 30.0);
        println!("60 fps game: inside {:.2}%", r * 100.0);
        assert!(r > 0.99, "only {:.1}% inside", r * 100.0);
    }

    #[test]
    fn bang_bang_mode_is_stable_too() {
        let mut cfg = Config::default();
        cfg.controller.mode = ControlMode::BangBang;
        let r = simulate(&cfg, &physics_240(), 30.0, 99);
        assert!(r > 0.9, "only {:.1}% inside", r * 100.0);
    }

    #[test]
    fn prediction_helps() {
        let phys = Physics {
            accel: 4000.0,
            fish_speed: 500.0,
            fish_accel: 3000.0,
            capture_age: 0.012,
            ..physics_240()
        };
        let cfg = Config::default();
        let mut blind = cfg.clone();
        blind.controller.lookahead_ms = 0.0;
        blind.controller.kd = 0.0;
        let with = mean(&cfg, &phys, 0..4, 20.0);
        let without = mean(&blind, &phys, 0..4, 20.0);
        assert!(with > without, "with {with} without {without}");
    }
}

//! Offline sanity check of the reel logic against a simulated minigame.
//!
//! The real Fisch physics are not public, so this is only a plausible model
//! (an accelerating bar, a fish that wanders to random targets, 60 Hz frames
//! and two frames of capture + input latency). It guards against regressions
//! that would make the controller unstable; tune on the real game afterwards.

#[cfg(test)]
mod tests {
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

    struct Physics {
        latency_frames: usize,
        /// Bar acceleration (px/s^2) while holding / releasing.
        accel: f64,
        max_speed: f64,
        /// Fish top speed and acceleration.
        fish_speed: f64,
        fish_accel: f64,
    }

    /// Returns the fraction of game frames in which the fish centre was inside the bar.
    fn simulate(cfg: &Config, phys: &Physics, seconds: f64, seed: u64) -> f64 {
        const W: f64 = 1000.0;
        let dt = 1.0 / 60.0;
        let mut rng = Rng(seed);
        let mut reel = Reel::new(cfg, W as f32);
        let (mut bar_x, mut bar_v) = (W / 2.0, 0.0);
        let (mut fish_x, mut fish_v) = (W / 2.0, 0.0);
        let mut target = W / 2.0;
        let mut next_target = 0.5;
        let mut pipeline = vec![false; phys.latency_frames];
        let (mut inside, mut total) = (0usize, 0usize);
        let frames = (seconds / dt) as usize;
        for k in 0..frames {
            let t = k as f64 * dt;
            // The bar shrinks from 90 px to 50 px over the reel, like the Ruinous Oath.
            let bar_w = 90.0 - 40.0 * (t / seconds);

            // Game physics with the input decided two frames ago.
            let hold = pipeline[0];
            pipeline.rotate_left(1);
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

            // What the macro sees one frame later, quantised to whole pixels.
            let bl = (bar_x - bar_w / 2.0).round() as f32;
            let br = (bar_x + bar_w / 2.0).round() as f32;
            let fl = (fish_x - 4.0).round() as f32;
            let det = Detection {
                bar: Some(Span {
                    left: bl,
                    right: br,
                }),
                fish: vec![Span {
                    left: fl,
                    right: fl + 8.0,
                }],
                progress: None,
            };
            let step = reel.step(t + dt + 0.001, &det, false);
            *pipeline.last_mut().unwrap() = step.hold;
        }
        inside as f64 / total as f64
    }

    fn physics() -> Physics {
        Physics {
            latency_frames: 2,
            accel: 3000.0,
            max_speed: 1500.0,
            fish_speed: 350.0,
            fish_accel: 1500.0,
        }
    }

    #[test]
    fn default_pd_tracks_a_wandering_fish() {
        let cfg = Config::default();
        for seed in [1, 7, 42, 1234] {
            let r = simulate(&cfg, &physics(), 30.0, seed);
            println!("seed {seed}: inside {:.2}%", r * 100.0);
            assert!(r > 0.995, "seed {seed}: only {:.1}% inside", r * 100.0);
        }
    }

    #[test]
    fn copes_with_more_latency_and_a_faster_fish() {
        let cfg = Config::default();
        let phys = Physics {
            latency_frames: 3,
            accel: 6000.0,
            max_speed: 2000.0,
            fish_speed: 600.0,
            fish_accel: 5000.0,
        };
        let r: f64 = (0..4)
            .map(|s| simulate(&cfg, &phys, 30.0, s * 31 + 5))
            .sum::<f64>()
            / 4.0;
        println!("hard case: inside {:.2}%", r * 100.0);
        assert!(r > 0.92, "only {:.1}% inside", r * 100.0);
    }

    #[test]
    fn bang_bang_mode_is_stable_too() {
        let mut cfg = Config::default();
        cfg.controller.mode = ControlMode::BangBang;
        let r = simulate(&cfg, &physics(), 30.0, 99);
        assert!(r > 0.9, "only {:.1}% inside", r * 100.0);
    }

    #[test]
    fn prediction_helps() {
        let cfg = Config::default();
        let mut no_lookahead = cfg.clone();
        no_lookahead.controller.lookahead_ms = 0.0;
        no_lookahead.controller.kd = 0.0;
        let with: f64 = (0..4)
            .map(|s| simulate(&cfg, &physics(), 20.0, s + 10))
            .sum();
        let without: f64 = (0..4)
            .map(|s| simulate(&no_lookahead, &physics(), 20.0, s + 10))
            .sum();
        assert!(with > without, "with {with} without {without}");
    }
}

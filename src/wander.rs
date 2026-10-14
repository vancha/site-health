// SPDX-License-Identifier: GPL-3.0

//! Slowly wanders the reaction's two knobs between a few chosen "species" of
//! pattern, so the organism keeps morphing: spots slowly become coral, coral
//! becomes stripes, and so on.

use crate::reaction::Rng;
use cosmic::iced::time::{Duration, Instant};

/// How long gliding from one species to the next takes when wandering by itself.
const GLIDE: Duration = Duration::from_secs(30);

/// How long gliding takes when skipping to the next species on purpose
/// (see `Wander::next`). Quick, but still a morph instead of a jump.
const QUICK_GLIDE: Duration = Duration::from_secs(3);

/// How long to stay at a species after arriving, before heading for the next.
const REST: Duration = Duration::from_secs(30);

/// One species of pattern: the `feed` and `kill` knobs that grow it.
#[derive(Clone, Copy)]
pub struct Species {
    pub feed: f32,
    pub kill: f32,
}

pub struct Wander {
    species: &'static [Species],
    /// Where the current glide started. Not necessarily a species from the
    /// list: skipping ahead mid-glide starts from wherever the knobs were.
    from: Species,
    /// The species currently being glided to (or rested at), as an index.
    to: usize,
    /// When the current glide started, and how long it takes.
    started: Instant,
    glide: Duration,
    rng: Rng,
}

impl Wander {
    /// Starts at a random species of the given list. The routes between all of
    /// them should be safe: gliding from any one to any other must not make
    /// the pattern die out or fill the dish.
    pub fn new(species: &'static [Species], now: Instant, seed: u64) -> Self {
        let mut rng = Rng::new(seed);
        let start = rng.below(species.len() as u32) as usize;

        Self {
            species,
            from: species[start],
            to: start,
            started: now,
            glide: GLIDE,
            rng,
        }
    }

    /// The knobs at this moment. Picks a random next species once it has
    /// rested long enough at the current one.
    pub fn knobs(&mut self, now: Instant) -> Species {
        if now.duration_since(self.started) >= self.glide + REST && self.species.len() > 1 {
            // Any species except the current one.
            let others = self.species.len() as u32 - 1;
            let next = (self.to + 1 + self.rng.below(others) as usize) % self.species.len();
            self.glide_to(next, GLIDE, now);
        }

        self.current(now)
    }

    /// Skips ahead to the next species in the list, quickly. Going through the
    /// list in order, rather than at random, makes it easy to see them all.
    pub fn next(&mut self, now: Instant) {
        let next = (self.to + 1) % self.species.len();
        self.glide_to(next, QUICK_GLIDE, now);
    }

    fn glide_to(&mut self, to: usize, glide: Duration, now: Instant) {
        // Start from wherever the knobs are right now, so nothing jumps.
        self.from = self.current(now);
        self.to = to;
        self.glide = glide;
        self.started = now;
    }

    /// The knobs at this moment of the current glide.
    fn current(&self, now: Instant) -> Species {
        let glided = now.duration_since(self.started).as_secs_f32() / self.glide.as_secs_f32();
        let t = glided.min(1.0);
        // Ease in and out, so it starts and arrives gently.
        let t = t * t * (3.0 - 2.0 * t);

        let to = self.species[self.to];
        Species {
            feed: self.from.feed + (to.feed - self.from.feed) * t,
            kill: self.from.kill + (to.kill - self.from.kill) * t,
        }
    }
}

// SPDX-License-Identifier: GPL-3.0

//! A panel icon that's a tiny living reaction-diffusion pattern — calm green
//! spots, dividing cells, coral and stripes when everything's fine, restless
//! churning red blobs and chaos when something isn't, each state slowly
//! morphing between a few variations of itself and drifting around the dish
//! — ported from the standalone `pulse_circle` applet. All the simulation and
//! animation bookkeeping lives behind [`PulseIcon`]; the caller only ever
//! needs [`PulseIcon::new`], [`PulseIcon::set_alarm`], [`PulseIcon::next_species`],
//! [`PulseIcon::update_theme`], [`PulseIcon::advance`], [`PulseIcon::frame_interval`],
//! and [`PulseIcon::image`].

use crate::reaction::Dish;
use crate::wander::{Species, Wander};
use cosmic::iced::animation::{Animation, Easing};
use cosmic::iced::time::{Duration, Instant};
use cosmic::iced::widget::image::{FilterMethod, Handle};
use cosmic::iced::{Color, Length};
use cosmic::widget;

/// The simulation grid is this many cells wide and tall. Bigger means smaller,
/// more numerous spots; this gives a handful of "cells" in the icon.
const GRID_SIZE: usize = 54;

/// How many times per second the organism lives a little and gets redrawn.
/// Every frame costs work, and this applet runs all day, so it uses as few as
/// look smooth: calm OK only moves slowly, so it gets by with fewer than
/// Alarm, whose quick heartbeat needs more to look smooth. Both are well
/// below the screen's own 60 or more.
const CALM_FRAMES_PER_SECOND: f64 = 20.0;
const ALARM_FRAMES_PER_SECOND: f64 = 30.0;

/// After the computer wakes from sleep, a lot of time has passed since the
/// last frame. Catch up on at most this much, instead of simulating all of it.
const MAX_CATCH_UP: Duration = Duration::from_millis(250);

/// How the little organism looks and behaves in one state.
struct Look {
    /// The species of pattern this state wanders between (see `wander.rs`).
    /// Every route between any two of them was tested to be safe: the
    /// pattern never dies out or fills the dish on the way.
    species: &'static [Species],
    /// How many simulation steps run per second: more steps, faster life.
    /// Per second rather than per frame, so the frame rate doesn't matter.
    steps_per_second: f32,
    /// How often a drop of fresh chemical lands in the dish (see `Dish::drop`).
    /// Without drops, calm patterns settle into a still arrangement after a
    /// while; restless ones need more of them, or they can nearly empty out.
    drop_every: Duration,
    /// Which of the theme's colors to use, so the organism matches the user's
    /// COSMIC theme. Read when the icon is created, and again whenever the
    /// theme changes (see `PulseIcon::update_theme`).
    color: fn(&cosmic::cosmic_theme::Theme) -> Color,
}

/// Everything is fine: calm, round shapes: spots, dividing cells and coral.
/// Lively enough to keep watching, but less than half as restless as Alarm.
const OK: Look = Look {
    species: &[
        Species { feed: 0.0367, kill: 0.0649 }, // dividing cells
        Species { feed: 0.030, kill: 0.062 },   // neat spots
        Species { feed: 0.0545, kill: 0.062 },  // coral
        Species { feed: 0.025, kill: 0.060 },   // spots that keep dividing and merging
        Species { feed: 0.026, kill: 0.0585 },  // wandering spots
    ],
    steps_per_second: 300.0,
    drop_every: Duration::from_millis(333),
    // The user's own accent color: "all is well" looks like the rest of their desktop.
    color: |theme| theme.accent_color().into(),
};

/// Something needs attention: restless red blobs and rings that keep churning.
const ALARM: Look = Look {
    species: &[
        Species { feed: 0.018, kill: 0.051 }, // churning blobs and rings
        Species { feed: 0.022, kill: 0.059 }, // fast-dividing spots
        Species { feed: 0.022, kill: 0.051 }, // turbulent
        Species { feed: 0.026, kill: 0.051 }, // chaos
    ],
    steps_per_second: 360.0,
    drop_every: Duration::from_millis(333),
    // The theme's color for danger, usually a red.
    color: |theme| theme.destructive_color().into(),
};

/// How long the view takes to drift back and forth across the pattern,
/// sideways and up-down. Two different, unrelated times make the drift wander
/// instead of going round in the same circle.
const DRIFT_PERIODS: (Duration, Duration) = (Duration::from_secs(47), Duration::from_secs(61));

/// How far the view drifts, as a fraction of the pattern's width.
const DRIFT_DISTANCE: f32 = 0.25;

/// OK breathes: the view slowly zooms in on the pattern and back out, like a
/// microscope gently refocusing. This long to zoom in, and as long to zoom out.
const BREATH_DURATION: Duration = Duration::from_secs(3);

/// How far OK's breath zooms in, as a fraction: 0.15 = 15% bigger.
const BREATH_ZOOM: f32 = 0.25;

/// In OK, the dish breathes along: breathed out it rests at this size (a
/// fraction of the icon), breathed in it fills the icon. Gentler than Alarm's
/// heartbeat (`HEARTBEAT_DISH_REST`). At panel icon sizes the dish is only
/// 12-16 pixels in radius, so small changes here are hard to see.
const BREATH_DISH_REST: f32 = 0.82;

/// Alarm has a heartbeat: "lub-dub", then a pause. One full beat takes this
/// long (about 70 beats per minute).
const HEARTBEAT_PERIOD: Duration = Duration::from_millis(850);

/// How far the "lub" of a heartbeat zooms in; the "dub" goes a bit less far.
const HEARTBEAT_ZOOM: f32 = 0.15;

/// In Alarm, the dish itself beats too: between beats it rests at this size
/// (a fraction of the icon), and each beat swells it out to the full icon.
const HEARTBEAT_DISH_REST: f32 = 0.74;

/// How strongly the heart is beating at this point of a beat (0.0 to 1.0 of
/// `HEARTBEAT_PERIOD`): two quick bumps, the second one smaller, then rest.
fn heartbeat(phase: f32) -> f32 {
    // A smooth bump: 1.0 at `center`, fading to 0.0 at `width` either side.
    let bump = |center: f32, width: f32| {
        let distance = ((phase - center) / width).abs();
        if distance >= 1.0 {
            0.0
        } else {
            0.5 + 0.5 * (distance * std::f32::consts::PI).cos()
        }
    };

    bump(0.10, 0.09) + 0.75 * bump(0.33, 0.09)
}

/// How long the switch between OK and Alarm takes. The knobs, speed and color
/// all blend over this time, so the pattern morphs instead of jumping.
const SWITCH_DURATION: Duration = Duration::from_millis(1500);

/// How long the organism takes to fade from its old colors to new ones after
/// the user's theme changes.
const THEME_FADE_DURATION: Duration = Duration::from_millis(1500);

/// A fade that starts at `now` and goes from 0.0 (old colors) to 1.0 (new colors).
fn theme_fade_from(now: Instant) -> Animation<bool> {
    Animation::new(false)
        .duration(THEME_FADE_DURATION)
        .easing(Easing::EaseInOut)
        .go(true, now)
}

/// Blends two numbers: `amount` 0.0 gives `from`, 1.0 gives `to`.
fn lerp(from: f32, to: f32, amount: f32) -> f32 {
    from + (to - from) * amount
}

/// Blends two colors: `amount` 0.0 gives `from`, 1.0 gives `to`.
fn mix(from: Color, to: Color, amount: f32) -> Color {
    Color::from_rgba(
        lerp(from.r, to.r, amount),
        lerp(from.g, to.g, amount),
        lerp(from.b, to.b, amount),
        lerp(from.a, to.a, amount),
    )
}

/// The animated OK/Alarm organism's own state, independent of whatever app
/// embeds it.
pub struct PulseIcon {
    /// The living pattern.
    dish: Dish,
    /// Whether we're in the Alarm state (`true`) or the OK state (`false`).
    alarm: bool,
    /// The switch between OK (`false`) and Alarm (`true`).
    switch: Animation<bool>,
    /// The slow wandering between species, one for each state. Both keep
    /// wandering all the time, so switching state blends between wherever
    /// each one currently is.
    ok_wander: Wander,
    alarm_wander: Wander,
    /// The colors for OK and Alarm, taken from the theme when the icon was
    /// created, or last updated by `update_theme`.
    ok_color: Color,
    alarm_color: Color,
    /// The colors as they were just before the most recent theme change, so
    /// `shown_colors` can fade away from them.
    old_ok_color: Color,
    old_alarm_color: Color,
    /// The fade from `old_ok_color`/`old_alarm_color` to `ok_color`/`alarm_color`.
    theme_fade: Animation<bool>,
    /// OK's slow breathing zoom (see `BREATH_DURATION`). Repeats forever and
    /// reverses after every run: in, out, in, out...
    breath: Animation<bool>,
    /// When this icon started existing; the drift is timed from here.
    born: Instant,
    /// When the last drop landed.
    last_drop: Instant,
    /// Leftover fraction of a simulation step, carried over to the next frame,
    /// so a speed like 4.5 steps per frame works out right on average.
    step_debt: f32,
    /// The moment of the latest frame.
    now: Instant,
}

impl PulseIcon {
    /// Starts in the OK state, with a different pattern every time (seeded
    /// from the current time).
    pub fn new(now: Instant) -> Self {
        let seed = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(1, |time| time.as_nanos() as u64);

        // The applet launcher has already loaded the user's theme by now.
        let theme = cosmic::theme::active();
        let theme = theme.cosmic();

        Self {
            dish: Dish::new(GRID_SIZE, seed),
            ok_wander: Wander::new(OK.species, now, seed.rotate_left(21)),
            alarm_wander: Wander::new(ALARM.species, now, seed.rotate_left(42)),
            ok_color: (OK.color)(theme),
            alarm_color: (ALARM.color)(theme),
            old_ok_color: (OK.color)(theme),
            old_alarm_color: (ALARM.color)(theme),
            // Already "finished": nothing to fade at startup.
            theme_fade: Animation::new(true),
            breath: Animation::new(false)
                .duration(BREATH_DURATION)
                .easing(Easing::EaseInOut)
                .repeat_forever()
                .auto_reverse()
                .go(true, now),
            alarm: false,
            switch: Animation::new(false)
                .duration(SWITCH_DURATION)
                .easing(Easing::EaseInOut),
            born: now,
            last_drop: now,
            step_debt: 0.0,
            now,
        }
    }

    /// How far the switch is right now: 0.0 = fully OK, 1.0 = fully Alarm.
    fn switch_progress(&self) -> f32 {
        self.switch.interpolate(0.0, 1.0, self.now)
    }

    /// Switches to the Alarm state (`true`) or the OK state (`false`) — the
    /// one place to call when something real changes, like a site going
    /// down (or back up).
    pub fn set_alarm(&mut self, alarm: bool) {
        self.alarm = alarm;
        self.switch.go_mut(alarm, self.now);
    }

    /// Skips to the next pattern species of whichever state we're currently
    /// in — call this on a click, for instance. Doesn't touch the OK/Alarm
    /// state itself, just which variation of it is showing.
    pub fn next_species(&mut self) {
        if self.alarm {
            self.alarm_wander.next(self.now);
        } else {
            self.ok_wander.next(self.now);
        }
    }

    /// The OK and Alarm colors as they look right now, partway through a fade
    /// to a new theme's colors if one is happening.
    fn shown_colors(&self) -> (Color, Color) {
        let faded = self.theme_fade.interpolate(0.0, 1.0, self.now);
        (
            mix(self.old_ok_color, self.ok_color, faded),
            mix(self.old_alarm_color, self.alarm_color, faded),
        )
    }

    /// Call whenever the system theme changes (see `cosmic::Application::system_theme_update`):
    /// starts a smooth fade from the colors currently on screen to the new theme's.
    pub fn update_theme(&mut self, new_theme: &cosmic::cosmic_theme::Theme) {
        let (ok, alarm) = self.shown_colors();
        self.old_ok_color = ok;
        self.old_alarm_color = alarm;

        self.ok_color = (OK.color)(new_theme);
        self.alarm_color = (ALARM.color)(new_theme);
        self.theme_fade = theme_fade_from(self.now);
    }

    /// How long this icon has existed, in seconds.
    fn age(&self) -> f32 {
        self.now.duration_since(self.born).as_secs_f32()
    }

    /// How strongly the heart is beating right now: 0.0 at rest, 1.0 at the
    /// peak of a "lub".
    fn heartbeat(&self) -> f32 {
        heartbeat((self.age() / HEARTBEAT_PERIOD.as_secs_f32()).fract())
    }

    /// How far OK's breath is right now: 0.0 = breathed out, 1.0 = breathed in.
    fn breath(&self) -> f32 {
        self.breath.interpolate(0.0, 1.0, self.now)
    }

    /// How big the dish is right now, as a fraction of the icon. In OK it
    /// breathes slowly; in Alarm it beats along with the heart.
    fn dish_size(&self) -> f32 {
        let breathing = BREATH_DISH_REST + (1.0 - BREATH_DISH_REST) * self.breath();
        let beating = HEARTBEAT_DISH_REST + (1.0 - HEARTBEAT_DISH_REST) * self.heartbeat();
        lerp(breathing, beating, self.switch_progress())
    }

    /// How far the view is zoomed in right now: 1.0 = not at all. OK breathes
    /// slowly, Alarm has a heartbeat, and switching state blends between them.
    fn zoom(&self) -> f32 {
        let breath = BREATH_ZOOM * self.breath();
        let beat = HEARTBEAT_ZOOM * self.heartbeat();
        1.0 + lerp(breath, beat, self.switch_progress())
    }

    /// Which part of the pattern is in view right now. The pattern wraps
    /// around, so drifting the view never runs off its edge.
    fn drift(&self) -> (f32, f32) {
        let wave = |period: Duration| {
            (self.age() / period.as_secs_f32() * std::f32::consts::TAU).sin() * DRIFT_DISTANCE
        };
        (wave(DRIFT_PERIODS.0), wave(DRIFT_PERIODS.1))
    }

    /// Call on every `Message::Frame`: lets the organism live for the time
    /// since the last call (capped at `MAX_CATCH_UP`, so waking from sleep
    /// doesn't try to simulate the whole gap at once).
    pub fn advance(&mut self, now: Instant) {
        let elapsed = now.duration_since(self.now).min(MAX_CATCH_UP).as_secs_f32();
        self.now = now;

        let progress = self.switch_progress();
        let ok = self.ok_wander.knobs(self.now);
        let alarm = self.alarm_wander.knobs(self.now);

        let feed = lerp(ok.feed, alarm.feed, progress);
        let kill = lerp(ok.kill, alarm.kill, progress);

        self.step_debt += lerp(OK.steps_per_second, ALARM.steps_per_second, progress) * elapsed;
        while self.step_debt >= 1.0 {
            self.dish.step(feed, kill);
            self.step_debt -= 1.0;
        }
        self.dish.revive_if_empty();

        let drop_every = lerp(
            OK.drop_every.as_secs_f32(),
            ALARM.drop_every.as_secs_f32(),
            progress,
        );
        if self.now.duration_since(self.last_drop).as_secs_f32() >= drop_every {
            self.dish.drop();
            self.last_drop = self.now;
        }
    }

    /// How often the caller should ask for the next frame: more often while
    /// Alarm or mid-switch (so the heartbeat looks smooth), less often at
    /// rest in OK (the slow breath doesn't need as many).
    pub fn frame_interval(&self) -> Duration {
        let frames_per_second = if self.alarm || self.switch.is_animating(self.now) {
            ALARM_FRAMES_PER_SECOND
        } else {
            CALM_FRAMES_PER_SECOND
        };
        Duration::from_secs_f64(1.0 / frames_per_second)
    }

    /// The organism, sized to fit an applet icon of `width` x `height`, as a
    /// finished picture handed straight to the renderer — far cheaper than
    /// drawing every pixel as a shape through a canvas.
    pub fn image(&self, width: f32, height: f32) -> widget::Image<'_, Handle> {
        let (width, height) = (width.max(1.0) as usize, height.max(1.0) as usize);

        let (ok_color, alarm_color) = self.shown_colors();
        let color = mix(ok_color, alarm_color, self.switch_progress());
        let [red, green, blue, _] = color.into_rgba8();

        let (zoom, drift) = (self.zoom(), self.drift());
        let center_x = width as f32 / 2.0;
        let center_y = height as f32 / 2.0;
        let radius = center_x.min(center_y) * self.dish_size();

        let mut pixels = Vec::with_capacity(width * height * 4);
        for py in 0..height {
            for px in 0..width {
                let (x, y) = (px as f32 + 0.5, py as f32 + 0.5);

                // Soft edge: fully inside the dish = 1.0, outside = 0.0, and a
                // one-pixel fade in between so the circle looks smooth.
                let distance = ((x - center_x).powi(2) + (y - center_y).powi(2)).sqrt();
                let in_dish = (radius - distance).clamp(0.0, 1.0);

                let amount = if in_dish > 0.0 {
                    let b = self.dish.b_at(
                        (x / width as f32 - 0.5) / zoom + 0.5 + drift.0,
                        (y / height as f32 - 0.5) / zoom + 0.5 + drift.1,
                    );
                    edge(b / FULL_B) * in_dish * color.a
                } else {
                    0.0
                };

                pixels.extend_from_slice(&[red, green, blue, (amount * 255.0).round() as u8]);
            }
        }

        let handle = Handle::from_rgba(width as u32, height as u32, pixels);
        // Nearest-neighbor scaling keeps the pixels crisp on high-resolution
        // screens, instead of blurring them.
        widget::image(handle)
            .width(Length::Fixed(width as f32))
            .height(Length::Fixed(height as f32))
            .filter_method(FilterMethod::Nearest)
    }
}

/// How much chemical B counts as "fully colored". The pattern's B rarely goes
/// much higher than this.
const FULL_B: f32 = 0.35;

/// Where the edge of a shape is drawn, as a fraction of `FULL_B`: more than
/// this is colored, less is transparent.
const EDGE: f32 = 0.20;

/// How gradual the edge is, as a fraction of `FULL_B`. Small values give crisp
/// shapes; 0.0 would be a hard on/off cut, which looks jagged at icon size.
/// 1.0 would be the old, soft and glowing look.
const EDGE_SOFTNESS: f32 = 0.2;

/// 0.0 below the edge, 1.0 above it, with a smooth step of `EDGE_SOFTNESS`
/// in between.
fn edge(level: f32) -> f32 {
    if EDGE_SOFTNESS <= 0.0 {
        // A hard cut: no in-between at all (and no dividing by zero below).
        return if level >= EDGE { 1.0 } else { 0.0 };
    }
    let t = ((level - (EDGE - EDGE_SOFTNESS / 2.0)) / EDGE_SOFTNESS).clamp(0.0, 1.0);
    t * t * (3.0 - 2.0 * t)
}

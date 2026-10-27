//! A Pomodoro timer: focus rounds and breaks.
//!
//! - The state (phase, end time, rounds) is shared by every part.
//! - A timer (`Work::at`) ends the phase exactly on time, whatever screen
//!   is open, and says so with a notice.
//! - The screen redraws once a minute (every ten seconds in the last
//!   minute), never holds the device awake, and works by touch and by
//!   buttons.
//! - A status item shows the minutes left anywhere status lines show.

#![no_std]

extern crate alloc;

use alloc::borrow::Cow;
use alloc::boxed::Box;
use alloc::format;
use alloc::rc::Rc;
use alloc::string::String;
use core::cell::RefCell;

use paperpaw_sdk::builtin::{actions, events, menus};
use paperpaw_sdk::power::WakeHold;
use paperpaw_sdk::time::unix_seconds;
use paperpaw_sdk::event::Event;
use paperpaw_sdk::setting::Setting;
use paperpaw_sdk::ui::{self, Ink, Label, Stack, TextRole, UiStyle};
use paperpaw_sdk::widgets::Align;
use paperpaw_sdk::{
    ActionSpec, Canvas, Command, Cx, Env, Gesture, GestureKind, Handled, Id, Instant, Manifest,
    MenuEntry, Params, Plugin, Rect, RegisterError, Registrar, Screen, ScreenSpec, SettingSpec,
    StatusItemSpec, StatusValue, Version,
};

pub const PLUGIN: Id = Id::new_static("pomodoro");
pub const OPEN: Id = Id::new_static("pomodoro.open");
pub const SCREEN: Id = Id::new_static("pomodoro.screen");
const DONE: Event<()> = Event::new("pomodoro.done");
const GROUP: Id = Id::new_static("pomodoro.group");

const FOCUS_MIN: Setting<i64> = Setting::new("pomodoro.focus");
const SHORT_MIN: Setting<i64> = Setting::new("pomodoro.short");
const LONG_MIN: Setting<i64> = Setting::new("pomodoro.long");
const ROUNDS: Setting<i64> = Setting::new("pomodoro.rounds");

static SETTINGS: [SettingSpec; 4] = [
    SettingSpec::number(FOCUS_MIN.static_str(), "pomodoro.setting.focus", 1, 90, 25, GROUP),
    SettingSpec::number(SHORT_MIN.static_str(), "pomodoro.setting.short", 1, 30, 5, GROUP),
    SettingSpec::number(LONG_MIN.static_str(), "pomodoro.setting.long", 1, 60, 15, GROUP),
    SettingSpec::number(ROUNDS.static_str(), "pomodoro.setting.rounds", 2, 8, 4, GROUP),
];

#[derive(Clone, Copy, PartialEq, Eq)]
enum Phase {
    Focus,
    Short,
    Long,
}

/// What every part shares.
struct State {
    phase: Phase,
    running: bool,
    /// When the phase ends while running.
    end: Option<Instant>,
    /// Milliseconds left while paused (or not started).
    left_ms: u64,
    /// Focus rounds finished in the current set.
    done: i64,
    /// Focus rounds finished since PaperPaw started.
    total: u32,
    /// Seconds since 1970 when the phase ends, while running: kept so a
    /// restart can go on where it was.
    end_unix: Option<i64>,
}

type Shared = Rc<RefCell<State>>;

/// Where the state is kept, in the plugin's own folder.
fn save_path() -> String {
    format!("{}/state.txt", Env::plugin_dir("pomodoro"))
}

fn phase_code(phase: Phase) -> u8 {
    match phase {
        Phase::Focus => 0,
        Phase::Short => 1,
        Phase::Long => 2,
    }
}

fn phase_from(code: i64) -> Phase {
    match code {
        1 => Phase::Short,
        2 => Phase::Long,
        _ => Phase::Focus,
    }
}

fn minutes(env: &Env<'_>, phase: Phase) -> u64 {
    let m = match phase {
        Phase::Focus => env.settings().read(&FOCUS_MIN),
        Phase::Short => env.settings().read(&SHORT_MIN),
        Phase::Long => env.settings().read(&LONG_MIN),
    };
    m.max(1) as u64
}

fn phase_key(phase: Phase) -> &'static str {
    match phase {
        Phase::Focus => "pomodoro.phase.focus",
        Phase::Short => "pomodoro.phase.short",
        Phase::Long => "pomodoro.phase.long",
    }
}

impl State {
    fn remaining(&self, now: Instant) -> u64 {
        if let (true, Some(end)) = (self.running, self.end) {
            end.since(now)
        } else {
            self.left_ms
        }
    }

    /// Keeps the state in a small file, so that switching the device off
    /// does not lose the round.
    fn save(&self, env: &Env<'_>) {
        let end = if self.running { self.end_unix.unwrap_or(0) } else { 0 };
        let text = format!(
            "{}|{}|{}|{}|{}|{}",
            phase_code(self.phase),
            u8::from(self.running),
            end,
            self.left_ms,
            self.done,
            self.total
        );
        let _ = env.storage().write(&save_path(), text.as_bytes());
    }

    /// Reads the kept state back at start-up. A round that was running
    /// goes on by the wall clock; one that ended meanwhile moves on.
    fn load(&mut self, env: &Env<'_>, out: &mut alloc::vec::Vec<Command>) {
        let mut buf = [0u8; 96];
        let Ok(n) = env.storage().read_at(&save_path(), 0, &mut buf) else {
            return;
        };
        let Ok(text) = core::str::from_utf8(&buf[..n]) else {
            return;
        };
        let mut parts = text.split('|').map(|p| p.trim().parse::<i64>().ok());
        let (Some(Some(phase)), Some(Some(running)), Some(Some(end)), Some(Some(left)), Some(Some(done)), Some(Some(total))) = (
            parts.next(),
            parts.next(),
            parts.next(),
            parts.next(),
            parts.next(),
            parts.next(),
        ) else {
            return;
        };
        self.phase = phase_from(phase);
        self.done = done.clamp(0, 99);
        self.total = total.clamp(0, 100_000) as u32;
        self.running = false;
        self.left_ms = (left.max(0) as u64).min(minutes(env, self.phase) * 60_000);
        if running == 1 && end > 0 {
            if let Some(now) = unix_seconds(env) {
                if end > now {
                    self.left_ms = ((end - now) as u64 * 1000)
                        .min(minutes(env, self.phase) * 60_000);
                    self.start(env, out);
                } else {
                    // Time ran out while the device was off.
                    self.running = true;
                    self.advance(env, true);
                }
            }
        }
    }

    fn start(&mut self, env: &Env<'_>, out: &mut alloc::vec::Vec<Command>) {
        if self.running {
            return;
        }
        self.running = true;
        let end = env.now().add_millis(self.left_ms);
        self.end = Some(end);
        self.end_unix = unix_seconds(env).map(|s| s + (self.left_ms / 1000) as i64);
        out.push(DONE.schedule((), end));
        self.save(env);
    }

    fn pause(&mut self, env: &Env<'_>, out: &mut alloc::vec::Vec<Command>) {
        if !self.running {
            return;
        }
        self.left_ms = self.remaining(env.now());
        self.running = false;
        out.push(Command::CancelWork(DONE.id().clone(), None));
        self.save(env);
    }

    fn enter(&mut self, env: &Env<'_>, phase: Phase) {
        self.phase = phase;
        self.running = false;
        self.left_ms = minutes(env, phase) * 60_000;
        self.save(env);
    }

    /// The phase after this one.
    fn next_phase(&self, env: &Env<'_>) -> Phase {
        match self.phase {
            Phase::Focus => {
                let every = env.settings().read(&ROUNDS).max(2);
                if self.done >= every {
                    Phase::Long
                } else {
                    Phase::Short
                }
            }
            _ => Phase::Focus,
        }
    }

    /// Finishes the phase (time is up, or skipped) and goes to the next.
    fn advance(&mut self, env: &Env<'_>, finished: bool) {
        if self.phase == Phase::Focus && finished {
            self.done += 1;
            self.total += 1;
        }
        let next = self.next_phase(env);
        if self.phase == Phase::Long {
            self.done = 0;
        }
        self.enter(env, next);
    }
}

#[derive(Debug, Default)]
pub struct Pomodoro;

static HOW: [(&str, &str); 2] = [
    ("en", include_str!("../how/en.txt")),
    ("de", include_str!("../how/de.txt")),
];

impl Plugin for Pomodoro {
    fn manifest(&self) -> Manifest {
        Manifest::new(PLUGIN, "Pomodoro", Version::new(0, 1, 0))
            .description("A Pomodoro timer for focused reading.")
            .how(&HOW)
            .uses(paperpaw_sdk::Uses::NOTHING)
    }

    fn register(&self, r: &mut Registrar) -> Result<(), RegisterError> {
        let shared: Shared = Rc::new(RefCell::new(State {
            phase: Phase::Focus,
            running: false,
            end: None,
            left_ms: 25 * 60_000,
            done: 0,
            total: 0,
            end_unix: None,
        }));

        // At start-up and on waking: go on where the saved state left off.
        let s = shared.clone();
        r.on(events::STARTED, move |env, _, out| {
            let mut cmds = alloc::vec::Vec::new();
            s.borrow_mut().load(env, &mut cmds);
            for c in cmds {
                out.push(c);
            }
        })?;
        let s = shared.clone();
        r.on(events::SUSPEND, move |env, _, _| {
            s.borrow().save(env);
        })?;

        r.add_action(ActionSpec::new(OPEN, "pomodoro.title").handler(|_, _, out| {
            out.push(Command::OpenScreen(SCREEN, Params::new()));
        }))?;
        r.add_menu_entry(MenuEntry::new(menus::TOOLS, OPEN).order(80))?;
        r.add_icon(OPEN, Cow::Borrowed("clock"))?;

        let made = shared.clone();
        r.add_screen(ScreenSpec::new(
            SCREEN,
            "pomodoro.title",
            Box::new(move |env, _| Box::new(PomodoroScreen::new(env, made.clone()))),
        ))?;

        // The time is up: tell the reader, go on to the next phase.
        let s = shared.clone();
        r.on(DONE, move |env, _, out| {
            let mut st = s.borrow_mut();
            if !st.running {
                return;
            }
            let was = st.phase;
            st.advance(env, true);
            let key = match was {
                Phase::Focus => "pomodoro.notice.break",
                _ => "pomodoro.notice.focus",
            };
            out.push(Command::Notify(String::from(env.i18n().tr(key))));
            // Breaks and the next round start by themselves only when asked.
            if env.settings().read(&AUTO) {
                let mut cmds = alloc::vec::Vec::new();
                st.start(env, &mut cmds);
                for c in cmds {
                    out.push(c);
                }
            }
        })?;

        r.add_setting_group(paperpaw_sdk::SettingGroupSpec::new(GROUP, "pomodoro.group"))?;
        r.add_icon(GROUP, Cow::Borrowed("clock"))?;
        r.add_settings(&SETTINGS)?;
        r.add_setting(SettingSpec::switch(
            AUTO.static_str(),
            "pomodoro.setting.auto",
            false,
            GROUP,
        ))?;
        r.add_setting(
            SettingSpec::switch(
                AWAKE.static_str(),
                "pomodoro.setting.awake",
                false,
                GROUP,
            )
            .description("pomodoro.setting.awake.help"),
        )?;

        // Minutes left, in any status line.
        let s = shared.clone();
        r.add_status_item(
            StatusItemSpec::new(
                Id::new_static("pomodoro.status"),
                "pomodoro.status",
                Box::new(move |env, _| {
                    let st = s.borrow();
                    if !st.running {
                        return None;
                    }
                    let mins = st.remaining(env.now()).div_ceil(60_000);
                    Some(StatusValue::Text(format!("{mins} min")))
                }),
            )
            .tap(OPEN),
        )?;

        r.add_translations(Cow::Borrowed(include_str!("../i18n/en.ini")))?;
        r.add_translations(Cow::Borrowed(include_str!("../i18n/de.ini")))?;
        Ok(())
    }
}

const AUTO: Setting<bool> = Setting::new("pomodoro.auto");
/// Keeps the screen on while a round runs and this screen is open.
const AWAKE: Setting<bool> = Setting::new("pomodoro.awake");

/// Milliseconds until the screen is drawn again: to the next full minute
/// of what is left (at least a second), every ten seconds in the last
/// minute.
fn next_tick(left_ms: u64) -> u64 {
    if left_ms <= 60_000 {
        10_000.min(left_ms.max(1000))
    } else {
        let into = left_ms % 60_000;
        if into == 0 { 60_000 } else { into }.max(1000)
    }
}

/// The seconds shown: whole minutes while more than a minute is left, in
/// steps of ten in the last one, so the clock never claims more than it
/// redraws.
fn shown_secs(left_ms: u64) -> u64 {
    let secs = left_ms.div_ceil(1000);
    if left_ms > 60_000 {
        secs.div_ceil(60) * 60
    } else {
        secs.div_ceil(10) * 10
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Focus {
    Main,
    Skip,
    Reset,
}

struct PomodoroScreen {
    style: UiStyle,
    header: ui::Header,
    shared: Shared,
    focus: Focus,
    buttons: [Rect; 3],
}

impl PomodoroScreen {
    fn new(env: &Env<'_>, shared: Shared) -> Self {
        let style = UiStyle::new(env);
        Self {
            header: ui::Header::new(env, &style, true),
            style,
            shared,
            focus: Focus::Main,
            buttons: [Rect::new(0, 0, 0, 0); 3],
        }
    }

    /// Asks for the next redraw as rarely as the display needs: once a
    /// minute, every ten seconds in the last minute. The device is never
    /// held awake; the phase's own timer ends it on time.
    fn tick(&self, cx: &mut Cx<'_, '_>) {
        let (running, left) = {
            let st = self.shared.borrow();
            (st.running, st.remaining(cx.env().now()))
        };
        if running {
            let now = cx.env().now();
            cx.wake_at(now.add_millis(next_tick(left)));
            // Only if the reader asked for it, and only while this screen
            // is open and the clock runs; the hold ends with the screen.
            if cx.env().settings().read(&AWAKE) {
                cx.keep_awake(WakeHold::new(
                    Id::new_static("pomodoro.awake"),
                    "pomodoro.setting.awake",
                ));
            }
        }
    }

    fn toggle(&mut self, cx: &mut Cx<'_, '_>) {
        let mut cmds = alloc::vec::Vec::new();
        {
            let mut st = self.shared.borrow_mut();
            if st.running {
                st.pause(cx.env(), &mut cmds);
            } else {
                st.start(cx.env(), &mut cmds);
                let note = cx.env().i18n().tr("pomodoro.notice.slow");
                cmds.push(Command::Notify(String::from(note)));
            }
        }
        for c in cmds {
            cx.command(c);
        }
        self.tick(cx);
        cx.request_redraw();
        cx.request_refresh(paperpaw_sdk::RefreshMode::Partial);
    }

    fn skip(&mut self, cx: &mut Cx<'_, '_>) {
        {
            let mut st = self.shared.borrow_mut();
            let was_running = st.running;
            st.advance(cx.env(), false);
            let _ = was_running;
        }
        cx.command(Command::CancelWork(DONE.id().clone(), None));
        self.shared.borrow().save(cx.env());
        cx.request_redraw();
        cx.request_refresh(paperpaw_sdk::RefreshMode::Partial);
    }

    fn reset(&mut self, cx: &mut Cx<'_, '_>) {
        {
            let mut st = self.shared.borrow_mut();
            st.done = 0;
            let env = cx.env();
            st.enter(env, Phase::Focus);
        }
        cx.command(Command::CancelWork(DONE.id().clone(), None));
        cx.request_redraw();
        cx.request_refresh(paperpaw_sdk::RefreshMode::Partial);
    }

    fn press(&mut self, which: Focus, cx: &mut Cx<'_, '_>) {
        match which {
            Focus::Main => self.toggle(cx),
            Focus::Skip => self.skip(cx),
            Focus::Reset => self.reset(cx),
        }
    }
}

impl Screen for PomodoroScreen {
    fn on_show(&mut self, cx: &mut Cx<'_, '_>) {
        self.tick(cx);
    }

    fn on_timer(&mut self, cx: &mut Cx<'_, '_>) {
        self.tick(cx);
        cx.request_redraw();
        cx.request_refresh(paperpaw_sdk::RefreshMode::Partial);
    }

    fn on_event(&mut self, _event: &Id, _params: &Params, cx: &mut Cx<'_, '_>) {
        // The phase may have ended while this screen is open.
        self.tick(cx);
        cx.request_redraw();
        cx.request_refresh(paperpaw_sdk::RefreshMode::Partial);
    }

    fn draw(&mut self, canvas: &mut Canvas<'_>, env: &Env<'_>) {
        let style = &self.style;
        let mut ui = ui::Ui::new(canvas, env, style);
        let i18n = env.i18n();
        self.header.draw(&mut ui, i18n.tr("pomodoro.title"), None, None);

        let display = env.device().display;
        let margin = style.margin();
        let top = self.header.area().height;
        let body = Rect::new(
            margin,
            top,
            display.width.saturating_sub(2 * margin),
            display.height.saturating_sub(top + margin),
        );

        let st = self.shared.borrow();
        let total_ms = minutes(env, st.phase) * 60_000;
        let left = st.remaining(env.now()).min(total_ms);
        let secs = shown_secs(left);
        let clock = format!("{:02}:{:02}", secs / 60, secs % 60);

        let mut stack = Stack::new(body, style.gap());
        let phase_area = stack.take(style.row());
        Label::new(i18n.tr(phase_key(st.phase)))
            .role(TextRole::Body)
            .align(Align::Center)
            .draw(&mut ui, phase_area);

        // The big clock.
        let clock_h = (body.height / 3).max(style.row());
        let clock_area = stack.take(clock_h);
        Label::new(&clock)
            .role(TextRole::Numeral)
            .min(12.0)
            .align(Align::Center)
            .draw(&mut ui, clock_area);

        // Progress of the phase.
        let bar_h = style.row() / 2;
        let bar = stack.take(bar_h.max(8));
        let permille = if total_ms == 0 {
            0
        } else {
            (1000 - (left * 1000 / total_ms).min(1000)) as u16
        };
        ui.progress(bar, permille, style.ink(Ink::Text));

        // Rounds of this set, as "● ● ○ ○", and the day's count.
        let every = env.settings().read(&ROUNDS).max(2);
        let dots_area = stack.take(style.row());
        let radius = (dots_area.height / 4).max(6) as i32;
        let step = radius * 3;
        let width = step * (every as i32 - 1);
        let cx0 = i32::from(dots_area.x) + i32::from(dots_area.width) / 2 - width / 2;
        let cy = i32::from(dots_area.y) + i32::from(dots_area.height) / 2;
        let ink = style.ink(Ink::Text);
        for n in 0..every {
            let c = paperpaw_sdk::Point::new(cx0 + step * n as i32, cy);
            if n < st.done {
                ui.canvas().fill_circle(c, radius as u32, ink);
            } else {
                ui.canvas().stroke_circle(c, radius as u32, 2, ink);
            }
        }
        let total_text = i18n.plural(
            "pomodoro.total",
            i64::from(st.total),
            &[("count", paperpaw_sdk::Arg::Int(i64::from(st.total)))],
        );
        let total_area = stack.take(style.row());
        Label::new(&total_text)
            .role(TextRole::Small)
            .align(Align::Center)
            .draw(&mut ui, total_area);

        // Buttons.
        let main_label = if st.running {
            i18n.tr("pomodoro.pause")
        } else {
            i18n.tr("pomodoro.start")
        };
        let labels = [main_label, i18n.tr("pomodoro.skip"), i18n.tr("pomodoro.reset")];
        let cells_h = ui::button_cells_height(env, style, body.width, &labels, style.gap());
        let area = stack.take_bottom(cells_h);
        let cells = ui::button_cells(env, style, area, &labels, style.gap());
        let shown = ui::focus_shown(env);
        let order = [Focus::Main, Focus::Skip, Focus::Reset];
        for (i, cell) in cells.iter().enumerate().take(3) {
            self.buttons[i] = *cell;
            ui.button(*cell, labels[i], i == 0, shown && self.focus == order[i]);
        }
    }

    fn on_action(&mut self, action: &Id, cx: &mut Cx<'_, '_>) -> Handled {
        let order = [Focus::Main, Focus::Skip, Focus::Reset];
        let at = order.iter().position(|f| *f == self.focus).unwrap_or(0);
        if *action == actions::CONFIRM {
            let f = self.focus;
            self.press(f, cx);
            return Handled::Yes;
        }
        if *action == actions::LEFT || *action == actions::UP {
            self.focus = order[(at + 2) % 3];
            cx.request_redraw();
            return Handled::Yes;
        }
        if *action == actions::RIGHT || *action == actions::DOWN {
            self.focus = order[(at + 1) % 3];
            cx.request_redraw();
            return Handled::Yes;
        }
        if *action == actions::BACK {
            cx.command(Command::Back);
            return Handled::Yes;
        }
        Handled::No
    }

    fn on_gesture(&mut self, gesture: Gesture, cx: &mut Cx<'_, '_>) -> Handled {
        if gesture.kind != GestureKind::Tap {
            return Handled::No;
        }
        let order = [Focus::Main, Focus::Skip, Focus::Reset];
        for (i, rect) in self.buttons.iter().enumerate() {
            if rect.contains(gesture.start) {
                self.focus = order[i];
                self.press(order[i], cx);
                return Handled::Yes;
            }
        }
        if let Some(action) = self.header.tap(cx.env(), &self.style, gesture.start, None) {
            cx.command(Command::run(action));
            return Handled::Yes;
        }
        Handled::No
    }
}

// SPDX-License-Identifier: GPL-3.0

use crate::config::Config;
use crate::fl;
use cosmic::cosmic_config::{self, ConfigSet, CosmicConfigEntry};
use cosmic::iced::platform_specific::shell::wayland::commands::popup::destroy_popup;
use cosmic::iced::{Alignment, Subscription, futures, window::Id};
use cosmic::prelude::*;
use cosmic::widget;
use futures::SinkExt;
use futures::channel::mpsc;
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::{Mutex, broadcast};

/// Abstract Unix socket name (leading NUL = Linux's abstract namespace, no
/// filesystem entry) used so only one instance of this applet — one spawns
/// per `cosmic-panel` monitor — does real HTTP checks and notifications;
/// whichever binds it first is the leader, everyone else relays from it.
const LEADER_SOCKET_NAME: &str = "\0com.github.vancha.site_health.leader";

/// One slot per site in `sites` order, `None` until that site's first check completes.
type SharedStatuses = Arc<Mutex<Vec<Option<bool>>>>;
/// A single site's status as broadcast/pushed over the wire: `(domain, up)`.
type StatusUpdate = (String, bool);
/// The channel the health-check subscription sends `Message`s to the app on.
type MessageSender = mpsc::Sender<Message>;

/// One byte per status: 2 = not yet checked, 1 = up, 0 = down. Keeping "not
/// yet checked" distinct from "down" avoids a freshly-connected listener
/// flashing every site red before the first real check lands.
fn encode_status(status: Option<bool>) -> u8 {
    match status {
        None => 2,
        Some(true) => 1,
        Some(false) => 0,
    }
}

/// Inverse of `encode_status`; an unrecognized byte decodes as "not yet checked".
fn decode_status(byte: u8) -> Option<bool> {
    match byte {
        1 => Some(true),
        0 => Some(false),
        _ => None,
    }
}

/// One self-describing push: 1-byte domain length, the domain's UTF-8 bytes,
/// then 1-byte status — self-describing so a listener decodes correctly even
/// if its own site list is momentarily out of sync with the leader's.
fn encode_site_push(domain: &str, up: bool) -> Vec<u8> {
    let mut body = Vec::with_capacity(2 + domain.len());
    body.push(domain.len() as u8);
    body.extend_from_slice(domain.as_bytes());
    body.push(encode_status(Some(up)));
    body
}

/// Every already-known status as `(domain, status)` pairs — used to catch a
/// freshly-connected listener up, and to resync one that lagged behind live updates.
async fn known_statuses(sites: &[String], statuses: &SharedStatuses) -> Vec<StatusUpdate> {
    let statuses = statuses.lock().await;
    sites
        .iter()
        .zip(statuses.iter())
        .filter_map(|(domain, &status)| status.map(|up| (domain.clone(), up)))
        .collect()
}

/// Aborts the wrapped task on drop. `try_become_leader`'s accept loop is a
/// bare `tokio::spawn` — normally detached — but it holds the socket claim,
/// so it must die with the subscription that spawned it, not outlive it.
struct AbortOnDrop(tokio::task::JoinHandle<()>);

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// Tries to claim `LEADER_SOCKET_NAME`. On success, spawns the accept loop
/// and returns a guard that releases the claim when dropped. Returns `None`
/// if another instance already holds the claim.
fn try_become_leader(
    sites: Arc<Vec<String>>,
    statuses: SharedStatuses,
    updates: broadcast::Sender<StatusUpdate>,
) -> Option<AbortOnDrop> {
    let listener = tokio::net::UnixListener::bind(LEADER_SOCKET_NAME).ok()?;
    let handle = tokio::spawn(async move {
        loop {
            let Ok((mut conn, _)) = listener.accept().await else {
                continue;
            };
            let sites = Arc::clone(&sites);
            let statuses = Arc::clone(&statuses);
            let mut updates = updates.subscribe();
            tokio::spawn(async move {
                // Catch-up first (same wire format as the live pushes below), then live.
                for (domain, up) in known_statuses(&sites, &statuses).await {
                    if conn
                        .write_all(&encode_site_push(&domain, up))
                        .await
                        .is_err()
                    {
                        return; // listener disconnected
                    }
                }
                loop {
                    match updates.recv().await {
                        Ok((domain, up)) => {
                            if conn
                                .write_all(&encode_site_push(&domain, up))
                                .await
                                .is_err()
                            {
                                return; // listener disconnected
                            }
                        }
                        // We fell behind the broadcast channel's buffer — `statuses` is
                        // still authoritative and current, so just resend everything
                        // rather than trying to figure out what was missed.
                        Err(broadcast::error::RecvError::Lagged(_)) => {
                            for (domain, up) in known_statuses(&sites, &statuses).await {
                                if conn
                                    .write_all(&encode_site_push(&domain, up))
                                    .await
                                    .is_err()
                                {
                                    return;
                                }
                            }
                        }
                        Err(broadcast::error::RecvError::Closed) => return,
                    }
                }
            });
        }
    });
    Some(AbortOnDrop(handle))
}

/// Checks every site concurrently over HTTPS, reporting each result to the
/// popup and every connected listener as soon as it's ready.
async fn check_sites_as_leader(
    client: &reqwest::Client,
    sites: &[String],
    statuses: &SharedStatuses,
    updates: &broadcast::Sender<StatusUpdate>,
    channel: &MessageSender,
) {
    let checks = sites.iter().cloned().enumerate().map(|(index, site)| {
        let mut channel = channel.clone();
        let statuses = Arc::clone(statuses);
        let updates = updates.clone();
        async move {
            let up = client
                .get(format!("https://{site}"))
                .send()
                .await
                .map(|resp| resp.status().is_success())
                .unwrap_or(false);
            statuses.lock().await[index] = Some(up);
            // No receivers (no listeners connected right now) is a fine outcome,
            // not an error — ignored either way.
            let _ = updates.send((site.clone(), up));
            _ = channel.send(Message::CheckResult(site, up)).await;
        }
    });
    futures::future::join_all(checks).await;
}

/// Connects to the leader once and relays every push straight into
/// `Message::CheckResult` until the connection breaks, so the caller can
/// retry becoming leader.
async fn relay_from_leader(channel: &MessageSender) {
    let Ok(mut conn) = tokio::net::UnixStream::connect(LEADER_SOCKET_NAME).await else {
        return;
    };
    loop {
        let mut len_buf = [0u8; 1];
        if conn.read_exact(&mut len_buf).await.is_err() {
            return;
        }
        let mut domain_buf = vec![0u8; len_buf[0] as usize];
        if conn.read_exact(&mut domain_buf).await.is_err() {
            return;
        }
        let Ok(domain) = String::from_utf8(domain_buf) else {
            return; // leader wouldn't send invalid UTF-8 — treat it as a dead connection
        };
        let mut status_buf = [0u8; 1];
        if conn.read_exact(&mut status_buf).await.is_err() {
            return;
        }
        // The leader only ever pushes known statuses, so this should always decode —
        // but skip rather than disconnect if it somehow doesn't.
        let Some(up) = decode_status(status_buf[0]) else {
            continue;
        };
        let mut channel = channel.clone();
        _ = channel.send(Message::CheckResult(domain, up)).await;
    }
}

/// Built once and reused across subscription restarts (site/interval changes
/// rebuild it). Can't be an `AppModel` field since `Subscription::run_with`'s
/// builder must be a plain, non-capturing `fn`.
static HTTP_CLIENT: std::sync::LazyLock<reqwest::Client> = std::sync::LazyLock::new(|| {
    reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(15))
        .build()
        .expect("failed to build http client")
});

/// Strips a URL scheme and trailing slash, and lowercases the result, so
/// entries stored are canonical regardless of how the user typed them —
/// matches `url::Url`'s own lowercasing, which `AddSite` gets for free.
fn normalize_domain(input: &str) -> String {
    input
        .trim()
        .trim_start_matches("https://")
        .trim_start_matches("http://")
        .trim_end_matches('/')
        .to_lowercase()
}

/// Parses user input as a site URL, adding an `https://` scheme if none was given.
/// Returns `None` if it isn't a valid absolute http(s) URL with a real-looking host.
fn parse_site_url(input: &str) -> Option<url::Url> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return None;
    }
    let candidate = if trimmed.contains("://") {
        trimmed.to_string()
    } else {
        format!("https://{trimmed}")
    };
    let url = url::Url::parse(&candidate).ok()?;
    let host_looks_real = match url.host()? {
        // An IPv4/IPv6 literal is already fully verified by the parser itself — a
        // malformed one (e.g. "999.999.999.999") fails to parse at all, above.
        url::Host::Ipv4(_) | url::Host::Ipv6(_) => true,
        // Domain names get no such guarantee from the parser (a single label or a
        // trailing dot both parse fine), so apply our own shape check.
        url::Host::Domain(domain) => domain_looks_real(domain),
    };
    (matches!(url.scheme(), "http" | "https") && host_looks_real).then_some(url)
}

/// Whether a domain has at least two non-empty, dot-separated labels with an
/// alphabetic-looking TLD — rejects e.g. "uuu" (no dot), "uuu." (empty label after the
/// trailing dot), and "uuu.1" (non-alphabetic TLD). Not a real TLD check (no public
/// suffix list), just enough to catch obvious non-domains.
fn domain_looks_real(domain: &str) -> bool {
    let labels: Vec<&str> = domain.split('.').collect();
    labels.len() >= 2
        && labels.iter().all(|label| !label.is_empty())
        && labels
            .last()
            .is_some_and(|tld| tld.len() >= 2 && tld.chars().all(|c| c.is_ascii_alphabetic()))
}

/// The check interval's spin button step, in minutes.
const INTERVAL_STEP_MINUTES: u64 = 1;
/// The check interval's spin button minimum, in minutes.
const INTERVAL_MIN_MINUTES: u64 = 1;
/// The check interval's spin button maximum, in minutes.
const INTERVAL_MAX_MINUTES: u64 = 1440;

/// A monitored site and its most recently observed status.
#[derive(Debug, Clone)]
struct Site {
    domain: String,
    /// `None` until the first check completes, then `Some(true)` for up, `Some(false)`
    /// for down.
    up: Option<bool>,
}

/// The application model stores app-specific state used to describe its interface and
/// drive its logic.
#[derive(Default)]
pub struct AppModel {
    /// Application state which is managed by the COSMIC runtime.
    core: cosmic::Core,
    /// The popup id.
    popup: Option<Id>,
    /// Configuration data that persists between application runs.
    config: Config,
    /// Handle used to write config changes back to disk. `None` if the config system
    /// failed to initialize, in which case changes only last for this session.
    config_handler: Option<cosmic_config::Config>,
    /// The monitored sites and their current status.
    sites: Vec<Site>,
    /// Current text in the "add a site" input field.
    new_site_input: String,
    /// Whether this instance won the `LEADER_SOCKET_NAME` claim, and is therefore
    /// responsible for the actual HTTP checks and desktop notifications. Set once
    /// the health-check subscription reports in via `Message::LeaderElected`.
    is_leader: bool,
}

/// Messages emitted by the application and its widgets.
#[derive(Debug, Clone)]
pub enum Message {
    TogglePopup,
    PopupClosed(Id),
    UpdateConfig(Config),
    CheckResult(String, bool),
    SiteInputChanged(String),
    AddSite(url::Url),
    RemoveSite(String),
    SetIntervalMinutes(u64),
    LeaderElected,
}

impl AppModel {
    /// The currently-watched domains, in order.
    fn domain_list(&self) -> Vec<String> {
        self.sites.iter().map(|site| site.domain.clone()).collect()
    }

    /// Updates the in-memory site list and returns a task that persists it to
    /// disk on the runtime's executor, so a slow write can't stall the event loop.
    fn persist_sites(&mut self) -> Task<cosmic::Action<Message>> {
        let sites = self.domain_list();
        self.config.sites = sites.clone();
        let Some(handler) = self.config_handler.clone() else {
            return Task::none();
        };
        Task::future(async move {
            let _ = tokio::task::spawn_blocking(move || handler.set("sites", sites)).await;
        })
        .discard()
    }

    /// Updates the in-memory check interval and returns a task that persists it
    /// to disk on the runtime's executor, so a slow write can't stall the event loop.
    fn persist_interval(&mut self, secs: u64) -> Task<cosmic::Action<Message>> {
        self.config.check_interval_secs = secs;
        let Some(handler) = self.config_handler.clone() else {
            return Task::none();
        };
        Task::future(async move {
            let _ =
                tokio::task::spawn_blocking(move || handler.set("check_interval_secs", secs)).await;
        })
        .discard()
    }
}

/// Create a COSMIC application from the app model
impl cosmic::Application for AppModel {
    /// The async executor that will be used to run your application's commands.
    type Executor = cosmic::executor::Default;

    /// Data that your application receives to its init method.
    type Flags = ();

    /// Messages which the application and its widgets will emit.
    type Message = Message;

    /// Unique identifier in RDNN (reverse domain name notation) format.
    const APP_ID: &'static str = "com.github.vancha.site_health";

    fn core(&self) -> &cosmic::Core {
        &self.core
    }

    fn core_mut(&mut self) -> &mut cosmic::Core {
        &mut self.core
    }

    /// Initializes the application with any given flags and startup commands.
    fn init(
        core: cosmic::Core,
        _flags: Self::Flags,
    ) -> (Self, Task<cosmic::Action<Self::Message>>) {
        let config_handler = cosmic_config::Config::new(Self::APP_ID, Config::VERSION).ok();
        let config = config_handler
            .as_ref()
            .map(|context| match Config::get_entry(context) {
                Ok(config) => config,
                Err((_errors, config)) => {
                    // for why in errors {
                    //     tracing::error!(%why, "error loading app config");
                    // }

                    config
                }
            })
            .unwrap_or_default();

        // Construct the app model with the runtime's core.
        let app = AppModel {
            core,
            sites: config
                .sites
                .iter()
                .map(|s| Site {
                    domain: normalize_domain(s),
                    up: None,
                })
                .collect(),
            config,
            config_handler,
            ..Default::default()
        };

        (app, Task::none())
    }

    fn on_close_requested(&self, id: Id) -> Option<Message> {
        Some(Message::PopupClosed(id))
    }

    /// Describes the interface based on the current state of the application model.
    ///
    /// The applet's button in the panel will be drawn using the main view method.
    /// This view should emit messages to toggle the applet's popup window, which will
    /// be drawn using the `view_window` method.
    fn view(&self) -> Element<'_, Self::Message> {
        let any_down = self.sites.iter().any(|site| site.up == Some(false));

        let button = if any_down {
            let color = cosmic::iced::Color::from_rgb8(0xE0, 0x1B, 0x24);
            let icon = widget::icon::from_name("dialog-error-symbolic")
                .symbolic(true)
                .size(self.core.applet.suggested_size(true).0)
                .icon()
                .class(cosmic::theme::Svg::Custom(std::rc::Rc::new(
                    move |_theme: &cosmic::Theme| widget::svg::Style { color: Some(color) },
                )));
            self.core.applet.button_from_element(icon, true)
        } else {
            // Default symbolic styling, same as every other panel icon.
            self.core.applet.icon_button("emblem-ok-symbolic")
        };

        button.on_press(Message::TogglePopup).into()
    }

    /// The applet's popup window will be drawn using this view method. If there are
    /// multiple poups, you may match the id parameter to determine which popup to
    /// create a view for.
    fn view_window(&self, _id: Id) -> Element<'_, Self::Message> {
        let cosmic::cosmic_theme::Spacing {
            space_xxs, space_s, ..
        } = cosmic::theme::active().cosmic().spacing;

        // `widget::list_column()` isn't used here because it wraps its content in a
        // `Container::List`-styled container, which paints a flat, fully opaque
        // background — that hides the popup's own translucent backdrop entirely. A
        // plain column of `padded_control`-wrapped rows paints nothing of its own,
        // matching how the official applets build their popups. Dividers mark
        // boundaries between logical groups (sites / interval / add-site), not
        // between every individual row within a group.
        let mut content = widget::Column::new();

        for site in &self.sites {
            let status_text = match site.up {
                Some(true) => fl!("status-up"),
                Some(false) => fl!("status-down"),
                None => fl!("status-checking"),
            };
            let row_end = widget::Row::new()
                .push(widget::text(status_text))
                .push(
                    widget::button::icon(widget::icon::from_name("window-close-symbolic"))
                        .extra_small()
                        .on_press(Message::RemoveSite(site.domain.clone())),
                )
                .spacing(space_xxs)
                .align_y(Alignment::Center);
            content = content.push(cosmic::applet::padded_control(widget::settings::item(
                site.domain.clone(),
                row_end,
            )));
        }

        content = content.push(
            cosmic::applet::padded_control(widget::divider::horizontal::default())
                .padding([space_xxs, space_s]),
        );

        let parsed_url = parse_site_url(&self.new_site_input);

        let mut site_input = widget::text_input(fl!("domain-placeholder"), &self.new_site_input)
            .on_input(Message::SiteInputChanged)
            .on_submit_maybe(
                parsed_url
                    .clone()
                    .map(|url| move |_raw: String| Message::AddSite(url.clone())),
            );
        if !self.new_site_input.trim().is_empty() && parsed_url.is_none() {
            site_input = site_input.error(fl!("invalid-domain"));
        }

        let interval_minutes = self.config.check_interval_secs / 60;
        let interval_row = widget::settings::item(
            fl!("interval-label"),
            widget::spin_button::spin_button(
                fl!("interval-minutes", minutes = interval_minutes),
                interval_minutes,
                INTERVAL_STEP_MINUTES,
                INTERVAL_MIN_MINUTES,
                INTERVAL_MAX_MINUTES,
                Message::SetIntervalMinutes,
            ),
        );
        content = content.push(cosmic::applet::padded_control(interval_row));

        content = content.push(
            cosmic::applet::padded_control(widget::divider::horizontal::default())
                .padding([space_xxs, space_s]),
        );

        let add_site_row = widget::Row::new()
            .push(site_input)
            .push(
                widget::button::suggested(fl!("store-button"))
                    .on_press_maybe(parsed_url.map(Message::AddSite)),
            )
            .spacing(space_xxs);
        content = content.push(cosmic::applet::padded_control(add_site_row));

        content = content.padding([space_xxs, 0]);

        self.core.applet.popup_container(content).into()
    }

    /// Register subscriptions for this application.
    ///
    /// Subscriptions are long-lived async tasks running in the background which
    /// emit messages to the application through a channel. They may be conditionally
    /// activated by selectively appending to the subscription batch, and will
    /// continue to execute for the duration that they remain in the batch.
    fn subscription(&self) -> Subscription<Self::Message> {
        let watched_sites = self.domain_list();
        let interval_secs = self.config.check_interval_secs;
        Subscription::batch(vec![
            // Re-checks every site on `interval_secs`, restarting fresh whenever the
            // site list or interval changes. Only the leader (see `LEADER_SOCKET_NAME`)
            // makes real requests and notifies; everyone else relays from it.
            Subscription::run_with(
                (watched_sites, interval_secs),
                |(sites, interval_secs): &(Vec<String>, u64)| {
                    let sites = sites.clone();
                    let interval_secs = *interval_secs;
                    cosmic::iced::stream::channel(16, move |channel: MessageSender| async move {
                        let client = &*HTTP_CLIENT;
                        let sites = Arc::new(sites);
                        let statuses: SharedStatuses =
                            Arc::new(Mutex::new(vec![None; sites.len()]));
                        let (updates_tx, _) = broadcast::channel::<StatusUpdate>(64);
                        let mut is_leader = false;
                        // Held only for its `Drop` — releases the leader claim on
                        // reassignment or when this future is cancelled.
                        #[allow(unused_assignments)]
                        let mut leader_guard: Option<AbortOnDrop> = None;

                        loop {
                            // Retry each round until we win — a dead leader's claim
                            // releases instantly, so the next retry here takes over.
                            if !is_leader {
                                leader_guard = try_become_leader(
                                    Arc::clone(&sites),
                                    Arc::clone(&statuses),
                                    updates_tx.clone(),
                                );
                                is_leader = leader_guard.is_some();
                                if is_leader {
                                    let mut leader_channel = channel.clone();
                                    _ = leader_channel.send(Message::LeaderElected).await;
                                }
                            }

                            if is_leader {
                                check_sites_as_leader(
                                    client,
                                    &sites,
                                    &statuses,
                                    &updates_tx,
                                    &channel,
                                )
                                .await;
                                tokio::time::sleep(std::time::Duration::from_secs(interval_secs))
                                    .await;
                            } else {
                                // Blocks until the connection breaks or fails to
                                // establish — no per-tick reconnecting.
                                relay_from_leader(&channel).await;
                                tokio::time::sleep(std::time::Duration::from_secs(2)).await;
                            }
                        }
                    })
                },
            ),
            // Watch for application configuration changes.
            self.core()
                .watch_config::<Config>(Self::APP_ID)
                .map(|update| {
                    // for why in update.errors {
                    //     tracing::error!(?why, "app config error");
                    // }

                    Message::UpdateConfig(update.config)
                }),
        ])
    }

    /// Handles messages emitted by the application and its widgets.
    ///
    /// Tasks may be returned for asynchronous execution of code in the background
    /// on the application's async runtime. The application will not exit until all
    /// tasks are finished.
    fn update(&mut self, message: Self::Message) -> Task<cosmic::Action<Self::Message>> {
        match message {
            Message::UpdateConfig(config) => {
                // Rebuild the working site list from the incoming config (which may have
                // changed from another instance of this applet, e.g. a sibling panel on a
                // different monitor) — preserving each site's already-known status so an
                // unrelated change elsewhere doesn't flash it back to "Checking…".
                self.sites = config
                    .sites
                    .iter()
                    .map(|domain| {
                        let domain = normalize_domain(domain);
                        let up = self
                            .sites
                            .iter()
                            .find(|site| site.domain == domain)
                            .and_then(|site| site.up);
                        Site { domain, up }
                    })
                    .collect();
                self.config = config;
            }
            Message::CheckResult(domain, up) => {
                let Some(entry) = self.sites.iter_mut().find(|site| site.domain == domain) else {
                    return Task::none();
                };
                let prev = entry.up;
                entry.up = Some(up);

                // Notify on a real state change, and also on the very first check if the
                // site is already down (don't notify on a first check that's already up).
                let should_notify = match prev {
                    None => !up,
                    Some(prev_up) => prev_up != up,
                };

                if should_notify && self.is_leader {
                    let (urgency, summary, body) = if up {
                        (
                            "normal",
                            fl!("notify-up-summary", domain = domain.as_str()),
                            fl!("notify-up-body"),
                        )
                    } else {
                        (
                            "critical",
                            fl!("notify-down-summary", domain = domain.as_str()),
                            fl!("notify-down-body"),
                        )
                    };
                    let _ = std::process::Command::new("notify-send")
                        .arg("-u")
                        .arg(urgency)
                        .arg(summary)
                        .arg(body)
                        .spawn();
                }
            }
            Message::SiteInputChanged(value) => {
                self.new_site_input = value;
            }
            Message::AddSite(url) => {
                self.new_site_input.clear();
                if let Some(domain) = url.host_str() {
                    let domain = domain.to_string();
                    if !self.sites.iter().any(|site| site.domain == domain) {
                        self.sites.push(Site { domain, up: None });
                        return self.persist_sites();
                    }
                }
            }
            Message::RemoveSite(domain) => {
                self.sites.retain(|site| site.domain != domain);
                return self.persist_sites();
            }
            Message::SetIntervalMinutes(minutes) => {
                return self.persist_interval(minutes * 60);
            }
            Message::LeaderElected => {
                self.is_leader = true;
            }
            Message::TogglePopup => {
                return if let Some(p) = self.popup.take() {
                    destroy_popup(p)
                } else {
                    return cosmic::surface::surface_task(cosmic::surface::action::app_popup(
                        |_| Default::default(),
                        |app: &mut Self| {
                            let new_id = Id::unique();
                            app.popup = Some(new_id);
                            app.core.applet.get_popup_settings(
                                app.core.main_window_id().unwrap(),
                                new_id,
                                Some((1, 1)),
                                None,
                                None,
                            )
                        },
                        None,
                    ));
                };
            }
            Message::PopupClosed(id) => {
                if self.popup.as_ref() == Some(&id) {
                    self.popup = None;
                }
            }
        }
        Task::none()
    }

    fn style(&self) -> Option<cosmic::iced::theme::Style> {
        Some(cosmic::applet::style())
    }
}

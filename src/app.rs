// SPDX-License-Identifier: GPL-3.0

use crate::config::Config;
use cosmic::cosmic_config::{self, ConfigSet, CosmicConfigEntry};
use cosmic::iced::platform_specific::shell::wayland::commands::popup::destroy_popup;
use cosmic::iced::{futures, window::Id, Alignment, Subscription};
use cosmic::prelude::*;
use cosmic::widget;
use futures::SinkExt;

/// The HTTP client used by the health-check subscription, built once and reused
/// across restarts (adding/removing a site or changing the check interval restarts
/// the subscription, which would otherwise rebuild a fresh client — including its
/// TLS/DNS setup — every time). `Subscription::run_with`'s builder has to be a
/// plain `fn` with no captures, so this can't just be a field on `AppModel`; a
/// process-wide lazy static reaches the same "build once" goal instead.
fn http_client() -> &'static reqwest::Client {
    static CLIENT: std::sync::OnceLock<reqwest::Client> = std::sync::OnceLock::new();
    CLIENT.get_or_init(|| {
        reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(15))
            .build()
            .expect("failed to build http client")
    })
}

/// Strips a leading URL scheme and trailing slash, so entries stored (and requested
/// over HTTPS by the checker) are bare domains regardless of how the user typed them.
fn normalize_domain(input: &str) -> String {
    input
        .trim()
        .trim_start_matches("https://")
        .trim_start_matches("http://")
        .trim_end_matches('/')
        .to_string()
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
            let _ = tokio::task::spawn_blocking(move || handler.set("check_interval_secs", secs)).await;
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
                .map(|s| Site { domain: normalize_domain(s), up: None })
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
                .class(cosmic::theme::Svg::Custom(std::rc::Rc::new(move |_theme: &cosmic::Theme| {
                    widget::svg::Style { color: Some(color) }
                })));
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
        let cosmic::cosmic_theme::Spacing { space_xxs, space_s, .. } =
            cosmic::theme::active().cosmic().spacing;

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
                Some(true) => "Up",
                Some(false) => "Down",
                None => "Checking…",
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

        let mut site_input = widget::text_input("example.com", &self.new_site_input)
            .on_input(Message::SiteInputChanged)
            .on_submit_maybe(
                parsed_url
                    .clone()
                    .map(|url| move |_raw: String| Message::AddSite(url.clone())),
            );
        if !self.new_site_input.trim().is_empty() && parsed_url.is_none() {
            site_input = site_input.error("Doesn't look like a valid domain");
        }

        let interval_minutes = self.config.check_interval_secs / 60;
        let interval_row = widget::settings::item(
            "Interval",
            widget::spin_button::spin_button(
                format!("{interval_minutes} min"),
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
            .push(widget::button::suggested("Store").on_press_maybe(parsed_url.map(Message::AddSite)))
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
            // Periodically re-checks every currently-watched site and reports each result
            // through the channel. Runs an initial check immediately on startup, then again
            // every `interval_secs`. Restarts (with a fresh interval) whenever the site list
            // or the check interval changes, since `data` is part of the subscription's
            // identity.
            Subscription::run_with((watched_sites, interval_secs), |(sites, interval_secs): &(Vec<String>, u64)| {
                let sites = sites.clone();
                let interval_secs = *interval_secs;
                cosmic::iced::stream::channel(16, move |channel: futures::channel::mpsc::Sender<_>| async move {
                    let client = http_client();

                    loop {
                        // Check every site concurrently and report each result as soon as
                        // it's ready, rather than awaiting them one at a time — otherwise
                        // sites later in the list visibly lag behind earlier ones.
                        let checks = sites.iter().cloned().map(|site| {
                            let mut channel = channel.clone();
                            async move {
                                let up = client
                                    .get(format!("https://{site}"))
                                    .send()
                                    .await
                                    .map(|resp| resp.status().is_success())
                                    .unwrap_or(false);
                                _ = channel.send(Message::CheckResult(site, up)).await;
                            }
                        });
                        futures::future::join_all(checks).await;
                        tokio::time::sleep(std::time::Duration::from_secs(interval_secs)).await;
                    }
                })
            }),
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

                if should_notify {
                    let (urgency, summary, body) = if up {
                        ("normal", format!("🟢 {domain} is back UP"), "Site is responding normally again".to_string())
                    } else {
                        ("critical", format!("🔴 {domain} is DOWN"), "HTTPS check failed".to_string())
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

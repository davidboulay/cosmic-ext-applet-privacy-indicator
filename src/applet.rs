// SPDX-License-Identifier: GPL-3.0-only

use nix::sys::signal::{Signal, kill};
use nix::unistd::Pid;
use std::{
    borrow::Cow,
    cell::RefCell,
    collections::HashMap,
    path::PathBuf,
    rc::Rc,
    sync::{LazyLock, OnceLock},
    time::{Duration, Instant},
};

use cosmic::{
    Application, Apply, Element,
    app::{Core, Task},
    applet::padded_control,
    cosmic_config,
    cosmic_theme::palette::WithAlpha,
    iced::{
        Alignment, Background, Border, Length, Subscription,
        core::layout::Limits,
        core::text::Wrapping,
        stream::channel,
        window::{self, Id as PopupId},
    },
    iced_winit::commands::popup::{destroy_popup, get_popup},
    theme::{Container, Svg, Theme},
    widget::{
        Column, Row, button, container::Style as CtnStyle, divider, icon, layer_container,
        mouse_area, svg::Style as SvgStyle, text,
    },
};
use cosmic_time::{Timeline, anim, chain};

use inotify::EventMask;
use pipewire::{
    channel::Sender,
    context::ContextRc,
    main_loop::MainLoopRc,
    node::{Node, NodeListener},
};

use crate::{
    CONFIG_VERSION, Config,
    camera::{get_inotify, open_cameras, procs_using_camera},
};

static REC_ICON: LazyLock<crate::rec_icon::Id> = LazyLock::new(crate::rec_icon::Id::unique);
static PW_SENDER: OnceLock<Sender<u32>> = OnceLock::new();

#[derive(Debug, Clone, Default)]
pub struct AppInfo<'s> {
    pub name: Cow<'s, str>,
    /// A stream/device-specific label that distinguishes multiple entries from
    /// the same app (e.g. OBS's "Desktop Audio" vs "Mic/Aux", or "video0").
    pub detail: Option<Cow<'s, str>>,
    pub id: u32,
}

#[derive(Default)]
struct Shared {
    pub microphone: bool,
    pub screenshare: bool,
    pub camera: bool,
}

#[derive(Default)]
pub struct PrivacyIndicator {
    core: Core,
    timeline: Timeline,
    shared: Shared,
    microphones: HashMap<u32, (String, Option<String>)>,
    screenshares: HashMap<u32, (String, Option<String>)>,
    cameras: HashMap<PathBuf, (i32, i32)>,
    popup: Option<PopupId>,
    config: Config,
}

#[derive(Debug, Clone)]
pub enum Message {
    Tick,
    RecTick(Instant),
    ScreenShareAdd(u32, String, Option<String>),
    MicrophoneAdd(u32, String, Option<String>),
    /// Patches in a stream's `media.name` once the bound node reports it.
    NodeDetail(u32, Option<String>),
    PipeWireNodeRemove(u32),
    CameraOpen(PathBuf),
    CameraClose(PathBuf),
    CameraPrevious(HashMap<PathBuf, (i32, i32)>),
    CameraReset(PathBuf),
    DisconnectNode(u32),
    TogglePopup,
    ClosePopup(PopupId),
    KillProcess(u32),
    Config(Config),
}

impl Application for PrivacyIndicator {
    type Executor = cosmic::executor::Default;

    type Flags = Config;

    type Message = Message;

    const APP_ID: &'static str = "dev.DBrox.CosmicPrivacyIndicator";

    fn core(&self) -> &Core {
        &self.core
    }

    fn core_mut(&mut self) -> &mut Core {
        &mut self.core
    }

    fn init(core: Core, flags: Self::Flags) -> (Self, Task<Self::Message>) {
        let mut timeline = Timeline::new();
        timeline.set_chain(chain![REC_ICON]).start();

        let app = PrivacyIndicator {
            core,
            timeline,
            config: flags,
            ..Default::default()
        };

        (app, Task::none())
    }

    fn on_close_requested(&self, id: PopupId) -> Option<Self::Message> {
        if self.popup == Some(id) {
            Some(Message::ClosePopup(id))
        } else {
            None
        }
    }

    fn view(&self) -> Element<'_, Self::Message> {
        let horizontal = self.core.applet.is_horizontal();
        let size = self.core.applet.suggested_size(true);
        let pad = self.core.applet.suggested_padding(true);

        let Shared {
            microphone,
            screenshare,
            camera,
        } = self.shared;

        if !microphone && !screenshare && !camera {
            return self
                .core
                .applet
                .autosize_window("")
                .limits(Limits::NONE)
                .into();
        }

        let mut icons: Vec<Element<Self::Message>> =
            vec![anim![REC_ICON, &self.timeline, size.0].into()];

        let icon_style = Rc::new(|theme: &Theme| SvgStyle {
            color: Some(theme.cosmic().button_color().into()),
        });
        let indicator = |name: &str| {
            icon(icon::from_name(name).into())
                .class(Svg::Custom(icon_style.clone()))
                .size(size.0)
        };

        if camera {
            icons.push(indicator("camera-web-symbolic").into());
        }
        if microphone {
            icons.push(indicator("audio-input-microphone-symbolic").into());
        }
        if screenshare {
            icons.push(indicator("accessories-screenshot-symbolic").into());
        }

        let container_style = |theme: &Theme| {
            let cosmic = theme.cosmic();
            CtnStyle {
                background: Some(Background::Color(
                    cosmic.primary.base.with_alpha(0.5).into(),
                )),
                border: Border {
                    radius: cosmic.corner_radii.radius_xl.into(),
                    ..Default::default()
                },
                ..Default::default()
            }
        };

        let content = if horizontal {
            Row::with_children(icons)
                .spacing(pad.0)
                .apply(layer_container)
        } else {
            Column::with_children(icons)
                .spacing(pad.1)
                .apply(layer_container)
        }
        .padding(pad.0.min(pad.1))
        .class(Container::Custom(Box::new(container_style)));

        self.core
            .applet
            .autosize_window(mouse_area(content).on_press(Message::TogglePopup))
            .limits(Limits::NONE)
            .into()
    }

    fn view_window(&self, _id: window::Id) -> Element<'_, Self::Message> {
        let microphones: Vec<_> = self
            .microphones
            .iter()
            .map(|(&id, (name, detail))| AppInfo {
                name: name.into(),
                detail: detail.as_deref().map(Cow::from),
                id,
            })
            .collect();
        let screenshares: Vec<_> = self
            .screenshares
            .iter()
            .map(|(&id, (name, detail))| AppInfo {
                name: name.into(),
                detail: detail.as_deref().map(Cow::from),
                id,
            })
            .collect();
        let cameras: Vec<_> = self
            .cameras
            .keys()
            .flat_map(|path| {
                // Tag each entry with the device (e.g. "video0") so multiple
                // streams from the same process are distinguishable.
                let device = path.file_name().map(|s| s.to_string_lossy().into_owned());
                procs_using_camera(path).into_iter().map(move |mut app| {
                    app.detail = device.clone().map(Cow::Owned);
                    app
                })
            })
            .collect();

        let mut rows: Vec<Element<Self::Message>> = vec![];

        macro_rules! section {
            ($label:expr, $apps:expr, $id:ident) => {
                if !$apps.is_empty() {
                    if !rows.is_empty() {
                        rows.push(divider::horizontal::default().into());
                    }
                    rows.push(padded_control(text::heading($label)).into());
                    for app in $apps {
                        let kill_btn = button::destructive("Kill").on_press_maybe(if app.id > 0 {
                            Some(Message::$id(app.id))
                        } else {
                            None
                        });
                        // The app name, plus the stream/device detail on a
                        // second line. WordOrGlyph wrapping breaks even long
                        // unbroken tokens (e.g. reverse-DNS ids) onto new lines
                        // so the label never runs into the Kill button.
                        let mut label = Column::new().push(
                            text::body(app.name.to_string())
                                .width(Length::Fill)
                                .wrapping(Wrapping::WordOrGlyph),
                        );
                        if let Some(detail) = &app.detail {
                            label = label.push(
                                text::caption(detail.to_string())
                                    .width(Length::Fill)
                                    .wrapping(Wrapping::WordOrGlyph),
                            );
                        }
                        rows.push(
                            padded_control(
                                Row::new()
                                    .push(label.width(Length::Fill))
                                    .push(kill_btn)
                                    .spacing(8)
                                    .align_y(Alignment::Center),
                            )
                            .into(),
                        );
                    }
                }
            };
        }

        section!("Camera", cameras, KillProcess);
        section!("Microphone", microphones, DisconnectNode);
        section!("Screen Share", screenshares, DisconnectNode);

        self.core
            .applet
            .popup_container(Column::with_children(rows))
            .into()
    }

    fn update(&mut self, message: Self::Message) -> Task<Self::Message> {
        match message {
            Message::Tick => {
                self.shared = Shared {
                    microphone: !self.microphones.is_empty(),
                    screenshare: !self.screenshares.is_empty(),
                    camera: self
                        .cameras
                        .values()
                        .fold(0, |acc, (shares, min)| acc + shares - min)
                        > 0,
                };
            }
            Message::CameraPrevious(cameras) => {
                self.cameras = cameras;
            }
            Message::CameraOpen(path) => {
                self.cameras
                    .entry(path.clone())
                    .and_modify(|v| v.0 += 1)
                    .or_insert((1, 0));
            }
            Message::CameraClose(path) => {
                self.cameras
                    .entry(path.clone())
                    .and_modify(|v| {
                        v.0 -= 1;
                        v.1 = v.1.min(v.0);
                    })
                    .or_insert((0, 0));
            }
            Message::CameraReset(path) => {
                self.cameras.remove(&path);
            }
            Message::ScreenShareAdd(id, name, detail) => {
                self.screenshares.insert(id, (name, detail));
            }
            Message::MicrophoneAdd(id, name, detail) => {
                self.microphones.insert(id, (name, detail));
            }
            Message::NodeDetail(id, detail) => {
                // The node lives in whichever map its Add message populated.
                if let Some(entry) = self.microphones.get_mut(&id) {
                    entry.1 = detail.clone();
                }
                if let Some(entry) = self.screenshares.get_mut(&id) {
                    entry.1 = detail;
                }
            }
            Message::PipeWireNodeRemove(id) => {
                self.screenshares.remove(&id);
                self.microphones.remove(&id);
            }
            Message::RecTick(now) => {
                self.timeline.now(now);
            }
            Message::TogglePopup => {
                if let Some(id) = self.popup.take() {
                    return destroy_popup(id);
                }
                let new_id = window::Id::unique();
                self.popup = Some(new_id);
                let settings = self.core.applet.get_popup_settings(
                    self.core.main_window_id().unwrap_or(window::Id::RESERVED),
                    new_id,
                    None,
                    None,
                    None,
                );

                return get_popup(settings);
            }
            Message::ClosePopup(id) => {
                self.popup.take_if(|stored_id| stored_id == &id);
            }
            Message::DisconnectNode(id) => {
                if let Some(sender) = PW_SENDER.get() {
                    let _ = sender.send(id);
                }
            }
            Message::KillProcess(pid) => {
                if let Err(e) = kill(Pid::from_raw(pid.cast_signed()), Signal::SIGTERM) {
                    println!("Failed to kill process {pid}: {e}");
                }
            }
            Message::Config(config) => self.config = config,
        }
        Task::none()
    }

    fn subscription(&self) -> Subscription<Self::Message> {
        let pw_shares = Self::pipewire_subscription();
        let camera_shares = Self::inotify_subscription();
        let config = Self::config_subscription();
        let timeline = if self.config.animated {
            cosmic::iced::time::every(Duration::from_millis(self.config.refresh))
                .map(Message::RecTick)
        } else {
            Subscription::none()
        };
        let tick = cosmic::iced::time::every(Duration::from_secs(2)).map(|_| Message::Tick);

        Subscription::batch([pw_shares, camera_shares, config, timeline, tick])
    }

    fn style(&self) -> Option<cosmic::iced_runtime::Appearance> {
        Some(cosmic::applet::style())
    }
}

impl PrivacyIndicator {
    pub fn config_subscription() -> Subscription<Message> {
        struct ConfigSubscription;
        cosmic_config::config_subscription(
            std::any::TypeId::of::<ConfigSubscription>(),
            Self::APP_ID.into(),
            CONFIG_VERSION,
        )
        .map(|update| {
            if !update.errors.is_empty() {
                println!(
                    "errors loading config {:?}: {:?}",
                    update.keys, update.errors
                );
            }
            Message::Config(update.config)
        })
    }

    fn pipewire_subscription() -> Subscription<Message> {
        let pw = || {
            channel(100, |output| async {
                std::thread::spawn(move || {
                    pipewire::init();
                    let main_loop =
                        MainLoopRc::new(None).expect("Failed to create PipeWire main loop");
                    let context = ContextRc::new(&main_loop, None)
                        .expect("Failed to create PipeWire context");
                    let core = context
                        .connect_rc(None)
                        .expect("Failed to connect to PipeWire");
                    let registry = core
                        .get_registry_rc()
                        .expect("Failed to get PipeWire registry");

                    let (sender, receiver) = pipewire::channel::channel::<u32>();
                    let _ = PW_SENDER.set(sender);

                    let receiver_registry = registry.clone();
                    let _attached = receiver.attach(main_loop.loop_(), move |id| {
                        receiver_registry.destroy_global(id);
                    });

                    let output_remove = output.clone();
                    // Keep the bound Node proxies + their info listeners alive so
                    // their `info` events keep firing; dropped on global_remove.
                    let bound: Rc<RefCell<HashMap<u32, (Node, NodeListener)>>> =
                        Rc::new(RefCell::new(HashMap::new()));
                    let bound_remove = bound.clone();
                    let bind_registry = registry.clone();
                    let _listener = registry
                        .add_listener_local()
                        .global(move |global| {
                            if global.type_.to_str() != "PipeWire:Interface:Node" {
                                return;
                            }
                            let Some(props) = global.props else { return };
                            let name = props
                                .get("application.name")
                                .or_else(|| props.get("node.name"))
                                .unwrap_or("Unknown")
                                .to_string();
                            let Some(media_class) = props.get("media.class") else {
                                return;
                            };
                            let msg = match media_class {
                                "Stream/Input/Video" => {
                                    Some(Message::ScreenShareAdd(global.id, name.clone(), None))
                                }
                                "Stream/Input/Audio" => {
                                    Some(Message::MicrophoneAdd(global.id, name.clone(), None))
                                }
                                _ => None,
                            };
                            if let Some(msg) = msg {
                                let mut out = output.clone();
                                loop {
                                    match out.try_send(msg.clone()) {
                                        Ok(()) => break,
                                        Err(_) => {
                                            eprintln!("Failed to send PipeWire event");
                                        }
                                    }
                                }
                                // `media.name` (the per-stream label, e.g. OBS's
                                // "Desktop Audio" vs "Mic/Aux") isn't in the
                                // registry global props — bind the node and read
                                // it from the node's info event, then patch it in.
                                if let Ok(node) = bind_registry.bind::<Node, _>(global) {
                                    let detail_out = output.clone();
                                    let node_id = global.id;
                                    let app_name = name;
                                    let listener = node
                                        .add_listener_local()
                                        .info(move |info| {
                                            let detail = info
                                                .props()
                                                .and_then(|p| {
                                                    p.get("media.name").map(str::to_string)
                                                })
                                                .filter(|d| !d.is_empty() && *d != app_name);
                                            let _ = detail_out
                                                .clone()
                                                .try_send(Message::NodeDetail(node_id, detail));
                                        })
                                        .register();
                                    bound.borrow_mut().insert(node_id, (node, listener));
                                }
                            }
                        })
                        .global_remove(move |id| {
                            bound_remove.borrow_mut().remove(&id);
                            let mut out = output_remove.clone();
                            loop {
                                match out.try_send(Message::PipeWireNodeRemove(id)) {
                                    Ok(()) => break,
                                    Err(_) => eprintln!("Failed to send unshare event"),
                                }
                            }
                        })
                        .register();
                    main_loop.run();
                });
            })
        };
        Subscription::run(pw)
    }

    fn inotify_subscription() -> Subscription<Message> {
        let inotify = || {
            channel(100, |mut output| async {
                std::thread::spawn(move || {
                    let open_cameras = open_cameras();
                    loop {
                        match output.try_send(Message::CameraPrevious(open_cameras.clone())) {
                            Ok(()) => break,
                            Err(_) => eprintln!("Failed to send previously open camera event"),
                        }
                    }
                    let (mut inotify, mut wd_path) = get_inotify();
                    let mut event_buffer = [0; 4096];

                    loop {
                        for event in inotify
                            .read_events_blocking(&mut event_buffer)
                            .expect("Failed to read events")
                        {
                            match event.mask {
                                EventMask::CREATE | EventMask::ATTRIB | EventMask::DELETE_SELF
                                    if (event.mask == EventMask::DELETE_SELF
                                        || event
                                            .name
                                            .unwrap_or_default()
                                            .to_string_lossy()
                                            .starts_with("video")) =>
                                {
                                    let old_wd_paths = wd_path;
                                    (inotify, wd_path) = get_inotify();
                                    let old_paths = old_wd_paths
                                        .left_values()
                                        .collect::<std::collections::HashSet<_>>();
                                    let new_paths = wd_path
                                        .left_values()
                                        .collect::<std::collections::HashSet<_>>();
                                    for &path in old_paths.difference(&new_paths) {
                                        loop {
                                            match output
                                                .try_send(Message::CameraReset(path.clone()))
                                            {
                                                Ok(()) => break,
                                                Err(_) => {
                                                    eprintln!("Failed to send camera reset event");
                                                }
                                            }
                                        }
                                    }
                                }
                                EventMask::OPEN => {
                                    wd_path.get_by_right(&event.wd).inspect(|&path| {
                                        let msg = Message::CameraOpen(path.clone());
                                        loop {
                                            match output.try_send(msg.clone()) {
                                                Ok(()) => break,
                                                Err(_) => {
                                                    eprintln!("Failed to send camera open event");
                                                }
                                            }
                                        }
                                    });
                                }
                                EventMask::CLOSE_WRITE | EventMask::CLOSE_NOWRITE => {
                                    wd_path.get_by_right(&event.wd).inspect(|&path| {
                                        let msg = Message::CameraClose(path.clone());
                                        loop {
                                            match output.try_send(msg.clone()) {
                                                Ok(()) => break,
                                                Err(_) => {
                                                    eprintln!("Failed to send camera close event");
                                                }
                                            }
                                        }
                                    });
                                }
                                _ => {}
                            }
                        }
                    }
                });
            })
        };
        Subscription::run(inotify)
    }
}

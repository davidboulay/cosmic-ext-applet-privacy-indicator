// SPDX-License-Identifier: GPL-3.0-only

use std::{
    collections::HashMap,
    path::PathBuf,
    rc::Rc,
    sync::LazyLock,
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
use pipewire::{context::ContextRc, main_loop::MainLoopRc};

use crate::{
    CONFIG_VERSION, Config,
    camera::{AppInfo, get_inotify, open_cameras, procs_using_camera},
};

static REC_ICON: LazyLock<crate::rec_icon::Id> = LazyLock::new(crate::rec_icon::Id::unique);

#[derive(Default)]
struct Shared {
    microphone: Vec<AppInfo>,
    screenshare: Vec<AppInfo>,
    camera: Vec<AppInfo>,
}

#[derive(Default)]
pub struct PrivacyIndicator {
    core: Core,
    timeline: Timeline,
    shared: Shared,
    microphones: HashMap<u32, AppInfo>,
    screenshares: HashMap<u32, AppInfo>,
    cameras: HashMap<PathBuf, (i32, i32)>,
    camera_apps: HashMap<PathBuf, Vec<AppInfo>>,
    popup: Option<PopupId>,
    config: Config,
}

#[derive(Debug, Clone)]
pub enum Message {
    Config(Config),
    Tick,
    RecTick(Instant),
    ScreenShareAdd(u32, AppInfo),
    MicrophoneAdd(u32, AppInfo),
    PipeWireNodeRemove(u32),
    CameraOpen(PathBuf, Vec<AppInfo>),
    CameraClose(PathBuf, Vec<AppInfo>),
    CameraPrevious(HashMap<PathBuf, (i32, i32)>),
    CameraReset(PathBuf),
    TogglePopup,
    ClosePopup(PopupId),
    KillProcess(u32),
}

impl PrivacyIndicator {
    fn refresh_shared(&mut self) {
        self.shared = Shared {
            microphone: self.microphones.values().cloned().collect(),
            screenshare: self.screenshares.values().cloned().collect(),
            camera: self
                .cameras
                .iter()
                .filter(|(_, (shares, min))| shares - min > 0)
                .flat_map(|(path, _)| self.camera_apps.get(path).cloned().unwrap_or_default())
                .collect(),
        };
    }
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
        } = &self.shared;

        if microphone.is_empty() && screenshare.is_empty() && camera.is_empty() {
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

        if !camera.is_empty() {
            icons.push(indicator("camera-web-symbolic").into());
        }
        if !microphone.is_empty() {
            icons.push(indicator("audio-input-microphone-symbolic").into());
        }
        if !screenshare.is_empty() {
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
        let Shared {
            microphone,
            screenshare,
            camera,
        } = &self.shared;

        let mut rows: Vec<Element<Self::Message>> = vec![];

        macro_rules! section {
            ($label:expr, $apps:expr) => {
                if !$apps.is_empty() {
                    if !rows.is_empty() {
                        rows.push(divider::horizontal::default().into());
                    }
                    rows.push(padded_control(text::heading($label)).into());
                    for app in $apps {
                        let kill_btn = button::destructive("Kill").on_press_maybe(if app.pid > 0 {
                            Some(Message::KillProcess(app.pid))
                        } else {
                            None
                        });
                        rows.push(
                            padded_control(
                                Row::new()
                                    .push(text::body(app.name.as_str()).width(Length::Fill))
                                    .push(kill_btn)
                                    .align_y(Alignment::Center),
                            )
                            .into(),
                        );
                    }
                }
            };
        }

        section!("Camera", camera);
        section!("Microphone", microphone);
        section!("Screen Share", screenshare);

        self.core
            .applet
            .popup_container(Column::with_children(rows))
            .into()
    }

    fn update(&mut self, message: Self::Message) -> Task<Self::Message> {
        match message {
            Message::Tick => {
                self.refresh_shared();
            }
            Message::CameraPrevious(cameras) => {
                self.cameras = cameras;
                self.refresh_shared();
            }
            Message::CameraOpen(path, apps) => {
                self.cameras
                    .entry(path.clone())
                    .and_modify(|v| v.0 += 1)
                    .or_insert((1, 0));
                self.camera_apps.insert(path, apps);
                self.refresh_shared();
            }
            Message::CameraClose(path, apps) => {
                self.cameras
                    .entry(path.clone())
                    .and_modify(|v| {
                        v.0 -= 1;
                        v.1 = v.1.min(v.0);
                    })
                    .or_insert((0, 0));
                self.camera_apps.insert(path, apps);
                self.refresh_shared();
            }
            Message::CameraReset(path) => {
                self.cameras.remove(&path);
                self.camera_apps.remove(&path);
                self.refresh_shared();
            }
            Message::ScreenShareAdd(id, info) => {
                self.screenshares.insert(id, info);
                self.refresh_shared();
            }
            Message::MicrophoneAdd(id, info) => {
                self.microphones.insert(id, info);
                self.refresh_shared();
            }
            Message::PipeWireNodeRemove(id) => {
                self.screenshares.remove(&id);
                self.microphones.remove(&id);
                self.refresh_shared();
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
            Message::KillProcess(pid) => {
                std::process::Command::new("kill")
                    .arg(pid.to_string())
                    .spawn()
                    .ok();
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
        Subscription::run(|| {
            channel(100, |output| async {
                std::thread::spawn(move || {
                    pipewire::init();
                    let main_loop =
                        MainLoopRc::new(None).expect("Failed to create PipeWire main loop");
                    let context = ContextRc::new(&main_loop, None)
                        .expect("Failed to create PipeWire context");
                    let core = context
                        .connect(None)
                        .expect("Failed to connect to PipeWire");
                    let registry = core
                        .get_registry()
                        .expect("Failed to get PipeWire registry");
                    let output_remove = output.clone();
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
                            let pid = props
                                .get("application.process.id")
                                .and_then(|s| s.parse::<u32>().ok())
                                .unwrap_or(0);
                            let info = AppInfo { name, pid };
                            let Some(media_class) = props.get("media.class") else {
                                return;
                            };
                            let msg = match media_class {
                                "Stream/Input/Video" => {
                                    Some(Message::ScreenShareAdd(global.id, info))
                                }
                                "Stream/Input/Audio" => {
                                    Some(Message::MicrophoneAdd(global.id, info))
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
                            }
                        })
                        .global_remove(move |id| {
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
        })
    }

    fn inotify_subscription() -> Subscription<Message> {
        Subscription::run(|| {
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
                                        let apps = procs_using_camera(path);
                                        let msg = Message::CameraOpen(path.clone(), apps);
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
                                        let apps = procs_using_camera(path);
                                        let msg = Message::CameraClose(path.clone(), apps);
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
        })
    }
}

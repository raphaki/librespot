use futures::{Async, Poll, Future, Stream, Sink};

use broadcast::BroadcastReceiver;
use connection::ConnectionChange;
use session::Session;
use types::*;
use player::{Player, PlayerEvent};
use protobuf::{self, Message};
use protocol;
use protocol::spirc::{Frame, DeviceState, MessageType, State, PlayStatus};
use util::SpotifyId;

mod command_sender;
use self::command_sender::CommandSender;

pub struct SpircManager {
    ident: String,

    session: Session,
    connection_updates: BroadcastReceiver<ConnectionChange>,
    seq_nr: u32,

    subscription: Option<SpStream<'static, Frame>>,
    sender: Option<SpSink<'static, Frame>>,

    state: SpircState,
    player: Player,
}

pub struct SpircState {
    name: String,
    volume: u16,

    is_active: bool,
    became_active_at: i64,
    status: PlayStatus,

    index: u32,
    tracks: Vec<SpotifyId>,

    update_id: i64,

    position_ms: u32,
    position_measured_at: u64,
}

impl SpircState {
    pub fn new(name: String) -> SpircState {
        SpircState {
            name: name,
            volume: 0xFFFF,

            is_active: false,
            became_active_at: 0,
            status: PlayStatus::kPlayStatusStop,

            index: 0,
            tracks: Vec::new(),

            update_id: 0,

            position_ms: 0,
            position_measured_at: 0,
        }
    }

    pub fn load_tracks(&mut self, state: &State) {
        self.index = state.get_playing_track_index();
        self.tracks = state.get_track().iter()
                           .filter(|track| track.has_gid())
                           .map(|track| SpotifyId::from_raw(track.get_gid()))
                           .collect();
    }
}

impl SpircManager {
    pub fn new(session: &Session, name: String) -> SpircManager {
        SpircManager {
            ident: session.device_id(),

            session: session.clone(),
            connection_updates: session.connection().updates(),
            seq_nr: 0,

            subscription: None,
            sender: None,

            state: SpircState::new(name),
            player: Player::new(session.clone()),
        }
    }

    fn build_subscription<'a>(&self, username: String) -> SpStream<'a, Frame> {
        let uri = format!("hm://remote/user/{}", username);
        let ident = self.ident.clone();

        self.session
            .mercury()
            .subscribe(uri)
            .flatten_stream()
            .and_then(|pkt| {
                let data = pkt.payload.first().unwrap();
                Ok(protobuf::parse_from_bytes::<Frame>(data)?)
            })
            .map(|frame| {
                debug!("{:?} {:?} {} {} {} {:?}",
                       frame.get_typ(),
                       frame.get_device_state().get_name(),
                       frame.get_ident(),
                       frame.get_seq_nr(),
                       frame.get_state_update_id(),
                       frame.get_recipient());
                frame
            })
            .filter(move |frame| {
                let recipients = frame.get_recipient();

                frame.get_ident() != ident && (recipients.len() == 0 || recipients.contains(&ident))
            })
            .sp_boxed()
    }

    fn build_sender<'a>(&self, username: String) -> SpSink<'a, Frame> {
        let uri = format!("hm://remote/user/{}", username);
        self.session
            .mercury()
            .sender(uri)
            .with(|frame: Frame| Ok(frame.write_to_bytes()?))
            .sp_boxed()
    }

    fn handle_connection(&mut self, username: String) {
        debug!("connected(username={:?})", username);

        self.subscription = Some(self.build_subscription(username.clone()));
        self.sender = Some(self.build_sender(username));

        self.command(MessageType::kMessageTypeHello).send();
    }

    fn process_frame(&mut self, frame: Frame) {
        if frame.get_state_update_id() > self.state.update_id {
            self.state.update_id = frame.get_state_update_id();
        }

        if frame.get_device_state().get_is_active() &&
            self.state.is_active &&
            frame.get_device_state().get_became_active_at() > self.state.became_active_at
        {
            self.state.is_active = false;
            self.state.status = PlayStatus::kPlayStatusStop;
            self.player.stop();

            self.notify(None);
        }

        let sender = frame.get_ident().to_owned();
        match frame.get_typ() {
            MessageType::kMessageTypeHello => self.notify(sender),

            MessageType::kMessageTypeVolume => {
                self.state.volume = frame.get_volume() as u16;
                self.notify(None);
            }

            MessageType::kMessageTypeLoad => {
                self.state.update_id = self.session.time() as i64;

                if !self.state.is_active {
                    self.state.is_active = true;
                    self.state.became_active_at = self.session.time() as i64;
                }

                self.state.load_tracks(frame.get_state());

                let track_index = self.state.index as usize;
                if track_index < self.state.tracks.len() {
                    let track_id = self.state.tracks[track_index];
                    self.player.load(track_id);

                    self.state.status = PlayStatus::kPlayStatusPlay;
                    self.state.position_ms = 0;
                    self.state.position_measured_at = self.session.time();
                }

                self.notify(None);
            }

            _ => (),
        }
    }

    fn next_seq(&mut self) -> u32 {
        self.seq_nr += 1;
        self.seq_nr
    }

    fn command(&mut self, cmd: MessageType) -> CommandSender {
        CommandSender::new(self, cmd)
    }

    fn send_frame(&mut self, frame: Frame) {
        if let Some(ref mut sender) = self.sender {
            sender.start_send(frame).expect("Send failed");
        } else {
            warn!("Not connected, dropping packet");
        }
    }

    fn notify<T>(&mut self, recipient: T)
        where T: Into<Option<String>>
    {
        let state = self.player_state();
        let update_id = self.state.update_id;
        self.command(MessageType::kMessageTypeNotify)
            .recipient(recipient)
            .state(state, update_id)
            .send();
    }

    pub fn player_state(&self) -> State {
        protobuf_init!(State::new(), {
            status: self.state.status,
            position_ms: self.state.position_ms,
            position_measured_at: self.state.position_measured_at,

            playing_track_index: self.state.index,
            track: self.state.tracks.iter().map(|track| {
                protobuf_init!(protocol::spirc::TrackRef::new(), {
                    gid: track.to_raw().to_vec()
                })
            }).collect(),

            playing_from_fallback: true,
        })
    }

    pub fn device_state(&self) -> DeviceState {
        protobuf_init!(DeviceState::new(), {
            sw_version: "librespot-v0.2",
            is_active: self.state.is_active,
            became_active_at: self.state.became_active_at,
            can_play: true,
            volume: self.state.volume as u32,
            name: self.state.name.clone(),
            error_code: 0,
            became_active_at: 0,
            capabilities => [
                @{
                    typ: protocol::spirc::CapabilityType::kCanBePlayer,
                    intValue => [0]
                },
                @{
                    typ: protocol::spirc::CapabilityType::kDeviceType,
                    intValue => [1]
                },
                @{
                    typ: protocol::spirc::CapabilityType::kGaiaEqConnectId,
                    intValue => [1]
                },
                @{
                    typ: protocol::spirc::CapabilityType::kSupportsLogout,
                    intValue => [1]
                },
                @{
                    typ: protocol::spirc::CapabilityType::kSupportsRename,
                    intValue => [1]
                },
                @{
                    typ: protocol::spirc::CapabilityType::kIsObservable,
                    intValue => [1]
                },
                @{
                    typ: protocol::spirc::CapabilityType::kVolumeSteps,
                    intValue => [10]
                },
                @{
                    typ: protocol::spirc::CapabilityType::kSupportedContexts,
                    stringValue => []
                },
                @{
                    typ: protocol::spirc::CapabilityType::kSupportedTypes,
                    stringValue => [
                        "audio/local",
                        "audio/track",
                        "local",
                        "track",
                    ]
                }
            ],
        })
    }
}

impl Future for SpircManager {
    type Item = ();
    type Error = SpError;

    fn poll(&mut self) -> Poll<(), SpError> {
        loop {
            let mut progress = false;

            let poll_connection = self.connection_updates.poll()?;
            if let Async::Ready(Some(change)) = poll_connection {
                let ConnectionChange::Connected(username) = change;
                self.handle_connection(username);
                progress = true;
            }

            let poll_subscription = self.subscription
                .as_mut()
                .map(Stream::poll)
                .unwrap_or(Ok(Async::NotReady))?;

            if let Async::Ready(Some(frame)) = poll_subscription {
                self.process_frame(frame);

                progress = true;
            }

            if let Some(ref mut sender) = self.sender {
                sender.poll_complete()?;
            }

            match self.player.poll()? {
                Async::Ready(Some(PlayerEvent::TrackEnd)) => {
                    self.state.index = (self.state.index + 1) % self.state.tracks.len() as u32;
                    let track_id = self.state.tracks[self.state.index as usize];
                    self.player.load(track_id);

                    self.state.update_id = self.session.time() as i64;
                    self.state.position_ms = 0;
                    self.state.position_measured_at = self.session.time();
                    self.notify(None);

                    progress = true;
                }
                Async::Ready(Some(PlayerEvent::Playing(position_ms))) => {
                    self.state.position_ms = position_ms;
                    self.state.position_measured_at = self.session.time();
                }
                _ => (),
            }

            if !progress {
                return Ok(Async::NotReady);
            }
        }
    }
}

//! Translation of the private C shim's GAP event view into owned events.
//!
//! The C shim copies the supported fields out of the borrowed, configuration-
//! sensitive `ble_gap_event` union into a fixed view. This module turns that
//! view into a typed [`GapEvent`]. Event and subscription codes are SDK
//! constants, so the translation takes them as [`GapCodes`] instead of
//! hard-coding values: the ESP backend supplies the generated constants and
//! host tests supply their own.

/// Plain copy of `struct argyle_nimble_gap_event_view`.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct GapEventView {
    pub(crate) kind: u32,
    pub(crate) subscribe_reason: u32,
    pub(crate) notify_enabled: bool,
    pub(crate) indicate_enabled: bool,
    pub(crate) connection: u16,
    pub(crate) channel: u16,
    pub(crate) attribute: u16,
    pub(crate) mtu: u16,
    pub(crate) status: i32,
    pub(crate) reason: i32,
    pub(crate) indication: bool,
}

/// SDK event and subscription-reason codes.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct GapCodes {
    pub(crate) connect: u32,
    pub(crate) disconnect: u32,
    pub(crate) connection_update: u32,
    pub(crate) advertising_complete: u32,
    pub(crate) notify_transmit: u32,
    pub(crate) subscribe: u32,
    pub(crate) mtu: u32,
    pub(crate) subscribe_write: u32,
    pub(crate) subscribe_terminated: u32,
    pub(crate) subscribe_restore: u32,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum SubscribeReason {
    /// The peer wrote the CCCD.
    Write,
    /// The connection ended.
    Terminated,
    /// Stored subscriptions were restored.
    Restore,
}

/// Supported peripheral-server GAP events. Status and reason values are SDK
/// status codes.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum GapEvent {
    Connect {
        connection: u16,
        status: i32,
    },
    Disconnect {
        connection: u16,
        reason: i32,
    },
    ConnectionUpdate {
        connection: u16,
        status: i32,
    },
    AdvertisingComplete {
        reason: i32,
    },
    NotifyTransmit {
        connection: u16,
        attribute: u16,
        status: i32,
        indication: bool,
    },
    Subscribe {
        connection: u16,
        attribute: u16,
        reason: SubscribeReason,
        notify: bool,
        indicate: bool,
    },
    Mtu {
        connection: u16,
        channel: u16,
        mtu: u16,
    },
}

impl GapEvent {
    /// Translate a view, or return `None` for an event kind or subscription
    /// reason this framework does not handle.
    pub(crate) fn from_view(view: &GapEventView, codes: &GapCodes) -> Option<Self> {
        let kind = view.kind;
        let event = if kind == codes.connect {
            Self::Connect {
                connection: view.connection,
                status: view.status,
            }
        } else if kind == codes.disconnect {
            Self::Disconnect {
                connection: view.connection,
                reason: view.reason,
            }
        } else if kind == codes.connection_update {
            Self::ConnectionUpdate {
                connection: view.connection,
                status: view.status,
            }
        } else if kind == codes.advertising_complete {
            Self::AdvertisingComplete {
                reason: view.reason,
            }
        } else if kind == codes.notify_transmit {
            Self::NotifyTransmit {
                connection: view.connection,
                attribute: view.attribute,
                status: view.status,
                indication: view.indication,
            }
        } else if kind == codes.subscribe {
            let reason = if view.subscribe_reason == codes.subscribe_write {
                SubscribeReason::Write
            } else if view.subscribe_reason == codes.subscribe_terminated {
                SubscribeReason::Terminated
            } else if view.subscribe_reason == codes.subscribe_restore {
                SubscribeReason::Restore
            } else {
                return None;
            };
            Self::Subscribe {
                connection: view.connection,
                attribute: view.attribute,
                reason,
                notify: view.notify_enabled,
                indicate: view.indicate_enabled,
            }
        } else if kind == codes.mtu {
            Self::Mtu {
                connection: view.connection,
                channel: view.channel,
                mtu: view.mtu,
            }
        } else {
            return None;
        };
        Some(event)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Deliberately not the SDK's values: the translation must use the codes
    /// it is given rather than assumptions.
    const CODES: GapCodes = GapCodes {
        connect: 40,
        disconnect: 41,
        connection_update: 42,
        advertising_complete: 43,
        notify_transmit: 44,
        subscribe: 45,
        mtu: 46,
        subscribe_write: 7,
        subscribe_terminated: 8,
        subscribe_restore: 9,
    };

    fn view(kind: u32) -> GapEventView {
        GapEventView {
            kind,
            subscribe_reason: 7,
            notify_enabled: true,
            indicate_enabled: false,
            connection: 3,
            channel: 4,
            attribute: 21,
            mtu: 247,
            status: -5,
            reason: 531,
            indication: true,
        }
    }

    #[test]
    fn each_supported_kind_keeps_only_its_documented_fields() {
        let cases = [
            (
                CODES.connect,
                GapEvent::Connect {
                    connection: 3,
                    status: -5,
                },
            ),
            (
                CODES.disconnect,
                GapEvent::Disconnect {
                    connection: 3,
                    reason: 531,
                },
            ),
            (
                CODES.connection_update,
                GapEvent::ConnectionUpdate {
                    connection: 3,
                    status: -5,
                },
            ),
            (
                CODES.advertising_complete,
                GapEvent::AdvertisingComplete { reason: 531 },
            ),
            (
                CODES.notify_transmit,
                GapEvent::NotifyTransmit {
                    connection: 3,
                    attribute: 21,
                    status: -5,
                    indication: true,
                },
            ),
            (
                CODES.subscribe,
                GapEvent::Subscribe {
                    connection: 3,
                    attribute: 21,
                    reason: SubscribeReason::Write,
                    notify: true,
                    indicate: false,
                },
            ),
            (
                CODES.mtu,
                GapEvent::Mtu {
                    connection: 3,
                    channel: 4,
                    mtu: 247,
                },
            ),
        ];
        for (kind, expected) in cases {
            assert_eq!(GapEvent::from_view(&view(kind), &CODES), Some(expected));
        }
    }

    #[test]
    fn subscription_reasons_map_and_unknown_reasons_are_rejected() {
        for (code, reason) in [
            (7, SubscribeReason::Write),
            (8, SubscribeReason::Terminated),
            (9, SubscribeReason::Restore),
        ] {
            let mut subscribe = view(CODES.subscribe);
            subscribe.subscribe_reason = code;
            assert!(matches!(
                GapEvent::from_view(&subscribe, &CODES),
                Some(GapEvent::Subscribe { reason: actual, .. }) if actual == reason
            ));
        }
        let mut unknown = view(CODES.subscribe);
        unknown.subscribe_reason = 99;
        assert_eq!(GapEvent::from_view(&unknown, &CODES), None);
    }

    #[test]
    fn unsupported_event_kinds_are_not_translated() {
        assert_eq!(GapEvent::from_view(&view(0), &CODES), None);
        assert_eq!(GapEvent::from_view(&view(u32::MAX), &CODES), None);
    }
}

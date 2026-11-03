//! Classic Outlook's calendar, read through its COM object model.
//!
//! The reader runs on one dedicated thread that joins a single-threaded
//! apartment, because the Outlook object model is STA. Polls are sent to that
//! thread over a channel and awaited on a oneshot, with a timeout so a hung
//! Outlook turns into an error instead of a stuck calendar task. Every COM
//! object is released at the end of each poll, so Outlook can exit while
//! Meetily keeps running.
//!
//! The appointment body is never read: the Outlook Object Model Guard can
//! raise a security dialog on it. The Teams join link comes from the named
//! MAPI property Teams writes on the appointment, then from the location text.
//!
//! Everything that interprets what Outlook returned is a pure function
//! outside the Windows module, so it is tested on every platform.

use super::{
    source::{CalendarSource, SourcePoll},
    teams_link::extract_teams_join_url,
    types::{CalendarEvent, SourceHealth, SourceKind},
};
use async_trait::async_trait;
use chrono::{DateTime, Duration, NaiveDate, NaiveDateTime, Utc};

/// Most appointments read in one poll. Two days of a busy calendar is a few
/// dozen; the cap only guards against a runaway recurrence expansion.
pub(crate) const MAX_ITEMS: usize = 500;

/// The named MAPI properties that carry an online meeting's join link, in the
/// order they are tried. Teams writes the first; the second is the older
/// Skype for Business slot that Teams also fills on some tenants.
#[cfg_attr(not(target_os = "windows"), allow(dead_code))]
pub(crate) const ONLINE_MEETING_URL_PROPERTIES: [&str; 2] = [
    "http://schemas.microsoft.com/mapi/string/{00020329-0000-0000-C000-000000000046}/SkypeTeamsMeetingUrl",
    "http://schemas.microsoft.com/mapi/string/{00020329-0000-0000-C000-000000000046}/OnlineMeetingExternalLink",
];

/// Shown when the subject is blank, so the nudge and the prompt have a title.
const UNTITLED: &str = "Untitled meeting";

/// What the COM thread reads from one `AppointmentItem`, before any
/// interpretation. Times are Outlook's local wall-clock values.
#[derive(Debug, Default, Clone, PartialEq)]
pub(crate) struct RawAppointment {
    pub global_id: String,
    pub subject: String,
    pub start: Option<NaiveDateTime>,
    pub end: Option<NaiveDateTime>,
    pub location: String,
    pub organizer: String,
    pub required_attendees: String,
    pub optional_attendees: String,
    pub is_recurring: bool,
    pub online_meeting_url: String,
}

/// An OLE automation date: days since 1899-12-30, fraction of a day as the
/// time. Outlook hands `Start` and `End` over in this form, in local time.
pub(crate) fn ole_date_to_naive(date: f64) -> Option<NaiveDateTime> {
    if !date.is_finite() {
        return None;
    }
    let epoch = NaiveDate::from_ymd_opt(1899, 12, 30)?.and_hms_opt(0, 0, 0)?;
    // Negative OLE dates keep the day in the integer part and the time in the
    // (positive) fraction; nothing in a calendar is before 1899 anyway.
    let days = date.trunc();
    let fraction = date.fract().abs();
    let seconds = (fraction * 86_400.0).round() as i64;
    epoch
        .checked_add_signed(Duration::days(days as i64))?
        .checked_add_signed(Duration::seconds(seconds))
}

/// The date form Outlook's `Items.Restrict` parses on an English locale.
pub(crate) fn restrict_date(local: NaiveDateTime) -> String {
    local.format("%m/%d/%Y %I:%M %p").to_string()
}

/// Appointments overlapping `[from, to]`, both local wall-clock times.
pub(crate) fn restrict_filter(from: NaiveDateTime, to: NaiveDateTime) -> String {
    format!(
        "[Start] <= '{}' AND [End] >= '{}'",
        restrict_date(to),
        restrict_date(from)
    )
}

/// Outlook lists attendees as display names separated by semicolons.
pub(crate) fn split_names(list: &str) -> Vec<String> {
    list.split(';')
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .map(str::to_string)
        .collect()
}

/// One calendar event from one appointment. `local_to_utc` is the machine's
/// zone on Windows and a fixed zone in tests. `None` when the appointment has
/// no usable start or end.
pub(crate) fn build_event(
    raw: &RawAppointment,
    local_to_utc: impl Fn(NaiveDateTime) -> Option<DateTime<Utc>>,
) -> Option<CalendarEvent> {
    let start = local_to_utc(raw.start?)?;
    let end = local_to_utc(raw.end?)?;
    if end < start {
        return None;
    }

    let id = if raw.is_recurring {
        format!("{}@{}", raw.global_id, start.to_rfc3339())
    } else {
        raw.global_id.clone()
    };

    let location = Some(raw.location.trim()).filter(|text| !text.is_empty()).map(str::to_string);
    let teams_join_url = extract_teams_join_url(&raw.online_meeting_url)
        .or_else(|| location.as_deref().and_then(extract_teams_join_url));

    let mut attendees = split_names(&raw.required_attendees);
    attendees.extend(split_names(&raw.optional_attendees));

    let subject = raw.subject.trim();
    Some(CalendarEvent {
        id,
        source: SourceKind::OutlookCom,
        title: if subject.is_empty() { UNTITLED.to_string() } else { subject.to_string() },
        start,
        end,
        organizer: Some(raw.organizer.trim()).filter(|text| !text.is_empty()).map(str::to_string),
        attendees,
        location,
        body: None,
        teams_join_url,
        is_recurring_instance: raw.is_recurring,
    })
}

/// Keeps the events inside the window, soonest first, one per id, capped.
/// Outlook's filter is applied in local time with minute precision, so the
/// exact window is re-checked here.
pub(crate) fn finish(mut events: Vec<CalendarEvent>, from: DateTime<Utc>, to: DateTime<Utc>) -> Vec<CalendarEvent> {
    events.retain(|event| event.end > from && event.start < to);
    events.sort_by(|a, b| a.start.cmp(&b.start).then_with(|| a.id.cmp(&b.id)));
    events.dedup_by(|a, b| a.id == b.id);
    events.truncate(MAX_ITEMS);
    events
}

pub struct OutlookComSource {
    #[cfg(target_os = "windows")]
    worker: platform::Worker,
}

impl OutlookComSource {
    pub fn new() -> Self {
        Self {
            #[cfg(target_os = "windows")]
            worker: platform::Worker::new(),
        }
    }
}

impl Default for OutlookComSource {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl CalendarSource for OutlookComSource {
    fn kind(&self) -> SourceKind {
        SourceKind::OutlookCom
    }

    #[cfg(target_os = "windows")]
    async fn poll(&self, from: DateTime<Utc>, to: DateTime<Utc>) -> SourcePoll {
        use chrono::{Local, TimeZone};

        let local_to_utc = |naive: NaiveDateTime| {
            Local
                .from_local_datetime(&naive)
                .earliest()
                .map(|local| local.with_timezone(&Utc))
        };
        let from_local = from.with_timezone(&Local).naive_local();
        let to_local = to.with_timezone(&Local).naive_local();

        match self.worker.read(from_local, to_local).await {
            Ok(raw) => {
                let events = raw.iter().filter_map(|item| build_event(item, local_to_utc)).collect();
                SourcePoll {
                    health: SourceHealth::Ok,
                    events: Some(finish(events, from, to)),
                }
            }
            Err(platform::ReadError::Unavailable(reason)) => {
                log::warn!("Outlook COM calendar unavailable: {reason}");
                SourcePoll {
                    health: SourceHealth::Unavailable { reason: reason.to_string() },
                    events: Some(Vec::new()),
                }
            }
            Err(platform::ReadError::Failed(reason)) => {
                log::warn!("Outlook COM calendar read failed: {reason}");
                SourcePoll {
                    health: SourceHealth::Error { reason },
                    events: None,
                }
            }
        }
    }

    #[cfg(not(target_os = "windows"))]
    async fn poll(&self, _from: DateTime<Utc>, _to: DateTime<Utc>) -> SourcePoll {
        SourcePoll {
            health: SourceHealth::Unavailable {
                reason: "Classic Outlook is only read on Windows.".to_string(),
            },
            events: Some(Vec::new()),
        }
    }
}

#[cfg(target_os = "windows")]
mod platform {
    //! Late-bound `IDispatch` calls against `Outlook.Application`. Property
    //! and method names are looked up by name on each call; the handful of
    //! calls per poll makes caching DISPIDs not worth the state.

    use super::{RawAppointment, MAX_ITEMS, ONLINE_MEETING_URL_PROPERTIES};
    use chrono::NaiveDateTime;
    use std::{
        mem::ManuallyDrop,
        sync::{mpsc, Mutex},
        time::Duration,
    };
    use tokio::sync::oneshot;
    use windows::{
        core::{w, Interface, BSTR, GUID, PCWSTR},
        Win32::{
            Foundation::{VARIANT_FALSE, VARIANT_TRUE},
            System::{
                Com::{
                    CLSIDFromProgID, CoCreateInstance, CoInitializeEx, CoUninitialize, IDispatch, CLSCTX_LOCAL_SERVER,
                    COINIT_APARTMENTTHREADED, DISPATCH_FLAGS, DISPATCH_METHOD, DISPATCH_PROPERTYGET,
                    DISPATCH_PROPERTYPUT, DISPPARAMS, EXCEPINFO,
                },
                Ole::{GetActiveObject, DISPID_PROPERTYPUT},
                Variant::{VariantClear, VARIANT, VT_BOOL, VT_BSTR, VT_DATE, VT_DISPATCH, VT_I4},
            },
        },
    };

    const LOCALE_USER_DEFAULT: u32 = 0x0400;
    /// `olFolderCalendar` in the Outlook object model.
    const FOLDER_CALENDAR: i32 = 9;
    /// A poll that takes longer than this is reported as an error; the
    /// worker thread finishes it in the background and the reply is dropped.
    const POLL_TIMEOUT: Duration = Duration::from_secs(30);

    pub(super) enum ReadError {
        /// Needs the user: Outlook is missing or would not start.
        Unavailable(&'static str),
        /// Transient; the previous event list is kept. Carries no page text.
        Failed(String),
    }

    struct Request {
        from: NaiveDateTime,
        to: NaiveDateTime,
        reply: oneshot::Sender<Result<Vec<RawAppointment>, ReadError>>,
    }

    /// Owns the STA thread. The thread is started on the first read and
    /// restarted if it ever went away.
    pub(super) struct Worker {
        sender: Mutex<Option<mpsc::Sender<Request>>>,
    }

    impl Worker {
        pub(super) fn new() -> Self {
            Self {
                sender: Mutex::new(None),
            }
        }

        pub(super) async fn read(
            &self,
            from: NaiveDateTime,
            to: NaiveDateTime,
        ) -> Result<Vec<RawAppointment>, ReadError> {
            let (reply, receiver) = oneshot::channel();
            self.send(Request { from, to, reply })?;
            match tokio::time::timeout(POLL_TIMEOUT, receiver).await {
                Ok(Ok(result)) => result,
                Ok(Err(_)) => Err(ReadError::Failed("The Outlook reader thread stopped.".to_string())),
                Err(_) => Err(ReadError::Failed(format!(
                    "Outlook did not answer within {} seconds.",
                    POLL_TIMEOUT.as_secs()
                ))),
            }
        }

        fn send(&self, request: Request) -> Result<(), ReadError> {
            let mut guard = self
                .sender
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let request = match guard.as_ref().map(|sender| sender.send(request)) {
                Some(Ok(())) => return Ok(()),
                Some(Err(mpsc::SendError(request))) => request,
                None => request,
            };
            let sender = spawn_thread().ok_or(ReadError::Failed("The Outlook reader thread could not start.".to_string()))?;
            sender
                .send(request)
                .map_err(|_| ReadError::Failed("The Outlook reader thread stopped.".to_string()))?;
            *guard = Some(sender);
            Ok(())
        }
    }

    fn spawn_thread() -> Option<mpsc::Sender<Request>> {
        let (sender, receiver) = mpsc::channel::<Request>();
        let spawned = std::thread::Builder::new()
            .name("outlook-com-calendar".into())
            .spawn(move || {
                let initialized = unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED) };
                if initialized.is_err() {
                    log::warn!("Outlook COM calendar: CoInitializeEx failed: {initialized:?}");
                    return;
                }
                for request in receiver {
                    let result = read_appointments(request.from, request.to);
                    let _ = request.reply.send(result);
                }
                unsafe { CoUninitialize() };
            });
        match spawned {
            Ok(_) => Some(sender),
            Err(error) => {
                log::warn!("Outlook COM calendar thread could not start: {error}");
                None
            }
        }
    }

    fn read_appointments(from: NaiveDateTime, to: NaiveDateTime) -> Result<Vec<RawAppointment>, ReadError> {
        let application = outlook_application()?;
        let namespace = application.call("GetNamespace", vec![text("MAPI")])?.object()?;
        let folder = namespace.call("GetDefaultFolder", vec![int(FOLDER_CALENDAR)])?.object()?;
        let items = folder.get("Items")?.object()?;
        items.put("IncludeRecurrences", boolean(true))?;
        items.call("Sort", vec![text("[Start]")])?;
        let filter = super::restrict_filter(from, to);
        let in_window = items.call("Restrict", vec![text(&filter)])?.object()?;

        let mut appointments = Vec::new();
        let mut current = in_window.call("GetFirst", vec![])?;
        while let Value::Object(item) = current {
            appointments.push(read_item(&item)?);
            if appointments.len() >= MAX_ITEMS {
                break;
            }
            current = in_window.call("GetNext", vec![])?;
        }
        Ok(appointments)
    }

    fn read_item(item: &Dispatch) -> Result<RawAppointment, ReadError> {
        let online_meeting_url = item
            .get("PropertyAccessor")
            .ok()
            .and_then(|value| value.object().ok())
            .and_then(|accessor| {
                ONLINE_MEETING_URL_PROPERTIES.iter().find_map(|schema| {
                    match accessor.call("GetProperty", vec![text(schema)]) {
                        Ok(Value::Text(url)) if !url.trim().is_empty() => Some(url),
                        _ => None,
                    }
                })
            })
            .unwrap_or_default();

        Ok(RawAppointment {
            global_id: item.get("GlobalAppointmentID")?.text(),
            subject: item.get("Subject")?.text(),
            start: item.get("Start")?.date().and_then(super::ole_date_to_naive),
            end: item.get("End")?.date().and_then(super::ole_date_to_naive),
            location: item.get("Location")?.text(),
            organizer: item.get("Organizer")?.text(),
            required_attendees: item.get("RequiredAttendees")?.text(),
            optional_attendees: item.get("OptionalAttendees")?.text(),
            is_recurring: item.get("IsRecurring")?.boolean(),
            online_meeting_url,
        })
    }

    /// A running classic Outlook first, then a new instance. Neither working
    /// means the user has to act, so both are `Unavailable`.
    fn outlook_application() -> Result<Dispatch, ReadError> {
        let class_id = unsafe { CLSIDFromProgID(w!("Outlook.Application")) }
            .map_err(|_| ReadError::Unavailable("Classic Outlook is not installed on this machine."))?;

        let mut running = None;
        if unsafe { GetActiveObject(&class_id, None, &mut running) }.is_ok() {
            if let Some(dispatch) = running.and_then(|unknown| unknown.cast::<IDispatch>().ok()) {
                return Ok(Dispatch(dispatch));
            }
        }

        unsafe { CoCreateInstance::<_, IDispatch>(&class_id, None, CLSCTX_LOCAL_SERVER) }
            .map(Dispatch)
            .map_err(|_| ReadError::Unavailable("Classic Outlook could not be started."))
    }

    fn failed(name: &str, error: windows::core::Error) -> ReadError {
        ReadError::Failed(format!("Outlook returned {:#010x} for {name}.", error.code().0))
    }

    /// A result read out of a `VARIANT`, with the variant released.
    enum Value {
        Empty,
        Text(String),
        Date(f64),
        Bool(bool),
        Int(i32),
        Object(Dispatch),
    }

    impl Value {
        fn take(mut variant: VARIANT) -> Self {
            let value = unsafe {
                let inner = &*variant.Anonymous.Anonymous;
                match inner.vt {
                    VT_BSTR => Value::Text((*inner.Anonymous.bstrVal).to_string()),
                    VT_DISPATCH => match &*inner.Anonymous.pdispVal {
                        Some(dispatch) => Value::Object(Dispatch(dispatch.clone())),
                        None => Value::Empty,
                    },
                    VT_DATE => Value::Date(inner.Anonymous.date),
                    VT_BOOL => Value::Bool(inner.Anonymous.boolVal != VARIANT_FALSE),
                    VT_I4 => Value::Int(inner.Anonymous.lVal),
                    _ => Value::Empty,
                }
            };
            let _ = unsafe { VariantClear(&mut variant) };
            value
        }

        fn text(self) -> String {
            match self {
                Value::Text(text) => text,
                _ => String::new(),
            }
        }

        fn date(self) -> Option<f64> {
            match self {
                Value::Date(date) => Some(date),
                _ => None,
            }
        }

        fn boolean(self) -> bool {
            matches!(self, Value::Bool(true))
        }

        fn object(self) -> Result<Dispatch, ReadError> {
            match self {
                Value::Object(dispatch) => Ok(dispatch),
                _ => Err(ReadError::Failed("Outlook returned no object where one was expected.".to_string())),
            }
        }
    }

    fn text(value: &str) -> VARIANT {
        let mut variant = VARIANT::default();
        unsafe {
            let inner = &mut *variant.Anonymous.Anonymous;
            inner.vt = VT_BSTR;
            inner.Anonymous.bstrVal = ManuallyDrop::new(BSTR::from(value));
        }
        variant
    }

    fn int(value: i32) -> VARIANT {
        let mut variant = VARIANT::default();
        unsafe {
            let inner = &mut *variant.Anonymous.Anonymous;
            inner.vt = VT_I4;
            inner.Anonymous.lVal = value;
        }
        variant
    }

    fn boolean(value: bool) -> VARIANT {
        let mut variant = VARIANT::default();
        unsafe {
            let inner = &mut *variant.Anonymous.Anonymous;
            inner.vt = VT_BOOL;
            inner.Anonymous.boolVal = if value { VARIANT_TRUE } else { VARIANT_FALSE };
        }
        variant
    }

    struct Dispatch(IDispatch);

    impl Dispatch {
        fn get(&self, name: &str) -> Result<Value, ReadError> {
            self.invoke(name, DISPATCH_PROPERTYGET, Vec::new(), false)
        }

        fn call(&self, name: &str, args: Vec<VARIANT>) -> Result<Value, ReadError> {
            self.invoke(name, DISPATCH_METHOD, args, false)
        }

        fn put(&self, name: &str, value: VARIANT) -> Result<(), ReadError> {
            self.invoke(name, DISPATCH_PROPERTYPUT, vec![value], true).map(|_| ())
        }

        fn dispid(&self, name: &str) -> Result<i32, ReadError> {
            let wide: Vec<u16> = name.encode_utf16().chain(std::iter::once(0)).collect();
            let names = [PCWSTR(wide.as_ptr())];
            let mut dispid = 0i32;
            unsafe {
                self.0
                    .GetIDsOfNames(&GUID::zeroed(), names.as_ptr(), 1, LOCALE_USER_DEFAULT, &mut dispid)
            }
            .map_err(|error| failed(name, error))?;
            Ok(dispid)
        }

        /// `args` in call order; COM wants them reversed. Every argument
        /// variant is released after the call, whatever the outcome.
        fn invoke(&self, name: &str, flags: DISPATCH_FLAGS, mut args: Vec<VARIANT>, property_put: bool) -> Result<Value, ReadError> {
            let dispid = self.dispid(name)?;
            args.reverse();
            let mut named_put = DISPID_PROPERTYPUT;
            let params = DISPPARAMS {
                rgvarg: args.as_mut_ptr(),
                rgdispidNamedArgs: if property_put { &mut named_put as *mut i32 } else { std::ptr::null_mut() },
                cArgs: args.len() as u32,
                cNamedArgs: if property_put { 1 } else { 0 },
            };
            let mut result = VARIANT::default();
            let mut exception: EXCEPINFO = unsafe { std::mem::zeroed() };
            let outcome = unsafe {
                self.0.Invoke(
                    dispid,
                    &GUID::zeroed(),
                    LOCALE_USER_DEFAULT,
                    flags,
                    &params,
                    Some(&mut result as *mut VARIANT),
                    Some(&mut exception as *mut EXCEPINFO),
                    None,
                )
            };
            for arg in args.iter_mut() {
                let _ = unsafe { VariantClear(arg) };
            }
            unsafe {
                ManuallyDrop::drop(&mut exception.bstrSource);
                ManuallyDrop::drop(&mut exception.bstrDescription);
                ManuallyDrop::drop(&mut exception.bstrHelpFile);
            }
            outcome.map_err(|error| failed(name, error))?;
            Ok(Value::take(result))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{TimeZone, Timelike};

    fn utc(naive: NaiveDateTime) -> Option<DateTime<Utc>> {
        Some(Utc.from_utc_datetime(&naive))
    }

    fn at(day: u32, hour: u32, minute: u32) -> NaiveDateTime {
        NaiveDate::from_ymd_opt(2026, 11, day).unwrap().and_hms_opt(hour, minute, 0).unwrap()
    }

    fn raw(subject: &str, start: NaiveDateTime, end: NaiveDateTime) -> RawAppointment {
        RawAppointment {
            global_id: "040000008200E00074C5B7101A82E008".to_string(),
            subject: subject.to_string(),
            start: Some(start),
            end: Some(end),
            ..RawAppointment::default()
        }
    }

    #[test]
    fn ole_dates_count_days_from_1899_with_the_time_as_a_fraction() {
        assert_eq!(ole_date_to_naive(0.0).unwrap(), NaiveDate::from_ymd_opt(1899, 12, 30).unwrap().and_hms_opt(0, 0, 0).unwrap());
        assert_eq!(ole_date_to_naive(2.5).unwrap(), NaiveDate::from_ymd_opt(1900, 1, 1).unwrap().and_hms_opt(12, 0, 0).unwrap());

        let target = at(3, 9, 30);
        let days = (target.date() - NaiveDate::from_ymd_opt(1899, 12, 30).unwrap()).num_days() as f64;
        let ole = days + (9.5 / 24.0);
        let parsed = ole_date_to_naive(ole).unwrap();
        assert_eq!(parsed.date(), target.date());
        assert_eq!((parsed.hour(), parsed.minute()), (9, 30));

        assert!(ole_date_to_naive(f64::NAN).is_none());
    }

    #[test]
    fn restrict_filter_uses_the_outlook_date_form() {
        assert_eq!(
            restrict_filter(at(3, 8, 5), at(5, 23, 0)),
            "[Start] <= '11/05/2026 11:00 PM' AND [End] >= '11/03/2026 08:05 AM'"
        );
    }

    #[test]
    fn attendee_lists_split_on_semicolons() {
        assert_eq!(
            split_names(" DAGHER Stephanie; YAMUNAN Aparna ;; "),
            vec!["DAGHER Stephanie".to_string(), "YAMUNAN Aparna".to_string()]
        );
        assert!(split_names("").is_empty());
    }

    #[test]
    fn join_link_comes_from_the_named_property_then_the_location() {
        let mut item = raw("Weekly sync", at(3, 9, 0), at(3, 9, 30));
        item.online_meeting_url = "https://teams.microsoft.com/l/meetup-join/19%3ameeting_abc%40thread.v2/0?context=x".to_string();
        item.location = "Microsoft Teams Meeting".to_string();
        let event = build_event(&item, utc).unwrap();
        assert_eq!(
            event.teams_join_url.as_deref(),
            Some("https://teams.microsoft.com/l/meetup-join/19%3ameeting_abc%40thread.v2/0?context=x")
        );
        assert!(event.is_teams_meeting());

        item.online_meeting_url.clear();
        item.location = "Room 4; https://teams.microsoft.com/meet/123456?p=abc".to_string();
        let event = build_event(&item, utc).unwrap();
        assert_eq!(event.teams_join_url.as_deref(), Some("https://teams.microsoft.com/meet/123456?p=abc"));

        item.location = "Microsoft Teams Meeting".to_string();
        let event = build_event(&item, utc).unwrap();
        assert_eq!(event.teams_join_url, None);
        assert!(event.is_teams_meeting(), "the location text alone still marks a Teams meeting");

        item.location = "Murex Building @ 100 Adelaide".to_string();
        assert!(!build_event(&item, utc).unwrap().is_teams_meeting());
    }

    #[test]
    fn recurring_instances_get_their_start_in_the_id() {
        let mut item = raw("Standup", at(3, 9, 0), at(3, 9, 15));
        let single = build_event(&item, utc).unwrap();
        assert_eq!(single.id, item.global_id);
        assert!(!single.is_recurring_instance);

        item.is_recurring = true;
        let instance = build_event(&item, utc).unwrap();
        assert_eq!(instance.id, format!("{}@2026-11-03T09:00:00+00:00", item.global_id));
        assert!(instance.is_recurring_instance);
    }

    #[test]
    fn people_and_title_are_tidied_and_the_body_stays_empty() {
        let mut item = raw("  ", at(3, 9, 0), at(3, 9, 30));
        item.organizer = " GUEVARA Marcos ".to_string();
        item.required_attendees = "A; B".to_string();
        item.optional_attendees = "C".to_string();
        let event = build_event(&item, utc).unwrap();
        assert_eq!(event.title, UNTITLED);
        assert_eq!(event.organizer.as_deref(), Some("GUEVARA Marcos"));
        assert_eq!(event.attendees, vec!["A", "B", "C"]);
        assert_eq!(event.body, None);
        assert_eq!(event.source, SourceKind::OutlookCom);
    }

    #[test]
    fn appointments_without_times_or_ending_before_starting_are_dropped() {
        let mut item = raw("Broken", at(3, 9, 0), at(3, 8, 0));
        assert!(build_event(&item, utc).is_none());
        item.end = None;
        assert!(build_event(&item, utc).is_none());
    }

    #[test]
    fn finish_keeps_the_window_sorted_and_unique() {
        let from = Utc.from_utc_datetime(&at(3, 8, 0));
        let to = Utc.from_utc_datetime(&at(5, 8, 0));
        let inside = build_event(&raw("b", at(4, 9, 0), at(4, 10, 0)), utc).unwrap();
        let mut earlier = build_event(&raw("a", at(3, 9, 0), at(3, 10, 0)), utc).unwrap();
        earlier.id = "earlier".to_string();
        let ended = build_event(&raw("old", at(3, 6, 0), at(3, 7, 0)), utc).unwrap();
        let later = build_event(&raw("far", at(5, 9, 0), at(5, 10, 0)), utc).unwrap();

        let kept = finish(vec![inside.clone(), later, earlier.clone(), ended, inside.clone()], from, to);
        assert_eq!(kept, vec![earlier, inside]);
    }

    #[test]
    fn finish_caps_the_list() {
        let events: Vec<CalendarEvent> = (0..(MAX_ITEMS + 20))
            .map(|index| {
                let mut event = build_event(&raw("x", at(3, 9, 0), at(3, 9, 30)), utc).unwrap();
                event.id = index.to_string();
                event
            })
            .collect();
        let from = Utc.from_utc_datetime(&at(3, 8, 0));
        let to = Utc.from_utc_datetime(&at(5, 8, 0));
        assert_eq!(finish(events, from, to).len(), MAX_ITEMS);
    }

    #[tokio::test]
    #[cfg(not(target_os = "windows"))]
    async fn off_windows_the_source_is_unavailable_with_no_events() {
        let source = OutlookComSource::new();
        let now = Utc::now();
        let poll = source.poll(now, now + Duration::hours(1)).await;
        assert!(matches!(poll.health, SourceHealth::Unavailable { .. }));
        assert_eq!(poll.events, Some(Vec::new()));
    }
}

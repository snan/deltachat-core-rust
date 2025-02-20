//! Location handling.
use std::convert::TryFrom;

use anyhow::{ensure, Context as _, Result};
use async_std::channel::Receiver;
use async_std::future::timeout;
use bitflags::bitflags;
use quick_xml::events::{BytesEnd, BytesStart, BytesText};
use std::time::Duration;

use crate::chat::{self, ChatId};
use crate::contact::ContactId;
use crate::context::Context;
use crate::dc_tools::{duration_to_str, time};
use crate::events::EventType;
use crate::message::{Message, MsgId, Viewtype};
use crate::mimeparser::SystemMessage;
use crate::stock_str;

/// Location record
#[derive(Debug, Clone, Default)]
pub struct Location {
    pub location_id: u32,
    pub latitude: f64,
    pub longitude: f64,
    pub accuracy: f64,
    pub timestamp: i64,
    pub contact_id: ContactId,
    pub msg_id: u32,
    pub chat_id: ChatId,
    pub marker: Option<String>,
    pub independent: u32,
}

impl Location {
    pub fn new() -> Self {
        Default::default()
    }
}

#[derive(Debug, Clone, Default)]
pub struct Kml {
    pub addr: Option<String>,
    pub locations: Vec<Location>,
    tag: KmlTag,
    pub curr: Location,
}

bitflags! {
    #[derive(Default)]
    struct KmlTag: i32 {
        const UNDEFINED = 0x00;
        const PLACEMARK = 0x01;
        const TIMESTAMP = 0x02;
        const WHEN = 0x04;
        const POINT = 0x08;
        const COORDINATES = 0x10;
    }
}

impl Kml {
    pub fn new() -> Self {
        Default::default()
    }

    pub fn parse(context: &Context, to_parse: &[u8]) -> Result<Self> {
        ensure!(to_parse.len() <= 1024 * 1024, "kml-file is too large");

        let mut reader = quick_xml::Reader::from_reader(to_parse);
        reader.trim_text(true);

        let mut kml = Kml::new();
        kml.locations = Vec::with_capacity(100);

        let mut buf = Vec::new();

        loop {
            match reader.read_event(&mut buf) {
                Ok(quick_xml::events::Event::Start(ref e)) => kml.starttag_cb(e, &reader),
                Ok(quick_xml::events::Event::End(ref e)) => kml.endtag_cb(e),
                Ok(quick_xml::events::Event::Text(ref e)) => kml.text_cb(e, &reader),
                Err(e) => {
                    error!(
                        context,
                        "Location parsing: Error at position {}: {:?}",
                        reader.buffer_position(),
                        e
                    );
                }
                Ok(quick_xml::events::Event::Eof) => break,
                _ => (),
            }
            buf.clear();
        }

        Ok(kml)
    }

    fn text_cb<B: std::io::BufRead>(&mut self, event: &BytesText, reader: &quick_xml::Reader<B>) {
        if self.tag.contains(KmlTag::WHEN) || self.tag.contains(KmlTag::COORDINATES) {
            let val = event.unescape_and_decode(reader).unwrap_or_default();

            let val = val
                .replace('\n', "")
                .replace('\r', "")
                .replace('\t', "")
                .replace(' ', "");

            if self.tag.contains(KmlTag::WHEN) && val.len() >= 19 {
                // YYYY-MM-DDTHH:MM:SSZ
                // 0   4  7  10 13 16 19
                match chrono::NaiveDateTime::parse_from_str(&val, "%Y-%m-%dT%H:%M:%SZ") {
                    Ok(res) => {
                        self.curr.timestamp = res.timestamp();
                        if self.curr.timestamp > time() {
                            self.curr.timestamp = time();
                        }
                    }
                    Err(_err) => {
                        self.curr.timestamp = time();
                    }
                }
            } else if self.tag.contains(KmlTag::COORDINATES) {
                let parts = val.splitn(2, ',').collect::<Vec<_>>();
                if let [longitude, latitude] = &parts[..] {
                    self.curr.longitude = longitude.parse().unwrap_or_default();
                    self.curr.latitude = latitude.parse().unwrap_or_default();
                }
            }
        }
    }

    fn endtag_cb(&mut self, event: &BytesEnd) {
        let tag = String::from_utf8_lossy(event.name()).trim().to_lowercase();

        if tag == "placemark" {
            if self.tag.contains(KmlTag::PLACEMARK)
                && 0 != self.curr.timestamp
                && 0. != self.curr.latitude
                && 0. != self.curr.longitude
            {
                self.locations
                    .push(std::mem::replace(&mut self.curr, Location::new()));
            }
            self.tag = KmlTag::UNDEFINED;
        };
    }

    fn starttag_cb<B: std::io::BufRead>(
        &mut self,
        event: &BytesStart,
        reader: &quick_xml::Reader<B>,
    ) {
        let tag = String::from_utf8_lossy(event.name()).trim().to_lowercase();
        if tag == "document" {
            if let Some(addr) = event
                .attributes()
                .filter_map(|a| a.ok())
                .find(|attr| String::from_utf8_lossy(attr.key).trim().to_lowercase() == "addr")
            {
                self.addr = addr.unescape_and_decode_value(reader).ok();
            }
        } else if tag == "placemark" {
            self.tag = KmlTag::PLACEMARK;
            self.curr.timestamp = 0;
            self.curr.latitude = 0.0;
            self.curr.longitude = 0.0;
            self.curr.accuracy = 0.0
        } else if tag == "timestamp" && self.tag.contains(KmlTag::PLACEMARK) {
            self.tag = KmlTag::PLACEMARK | KmlTag::TIMESTAMP
        } else if tag == "when" && self.tag.contains(KmlTag::TIMESTAMP) {
            self.tag = KmlTag::PLACEMARK | KmlTag::TIMESTAMP | KmlTag::WHEN
        } else if tag == "point" && self.tag.contains(KmlTag::PLACEMARK) {
            self.tag = KmlTag::PLACEMARK | KmlTag::POINT
        } else if tag == "coordinates" && self.tag.contains(KmlTag::POINT) {
            self.tag = KmlTag::PLACEMARK | KmlTag::POINT | KmlTag::COORDINATES;
            if let Some(acc) = event.attributes().find(|attr| {
                attr.as_ref()
                    .map(|a| String::from_utf8_lossy(a.key).trim().to_lowercase() == "accuracy")
                    .unwrap_or_default()
            }) {
                let v = acc
                    .unwrap()
                    .unescape_and_decode_value(reader)
                    .unwrap_or_default();

                self.curr.accuracy = v.trim().parse().unwrap_or_default();
            }
        }
    }
}

/// Enables location streaming in chat identified by `chat_id` for `seconds` seconds.
pub async fn send_locations_to_chat(
    context: &Context,
    chat_id: ChatId,
    seconds: i64,
) -> Result<()> {
    ensure!(seconds >= 0);
    ensure!(!chat_id.is_special());
    let now = time();
    let is_sending_locations_before = is_sending_locations_to_chat(context, Some(chat_id)).await?;
    context
        .sql
        .execute(
            "UPDATE chats    \
         SET locations_send_begin=?,        \
         locations_send_until=?  \
         WHERE id=?",
            paramsv![
                if 0 != seconds { now } else { 0 },
                if 0 != seconds { now + seconds } else { 0 },
                chat_id,
            ],
        )
        .await?;
    if 0 != seconds && !is_sending_locations_before {
        let mut msg = Message::new(Viewtype::Text);
        msg.text = Some(stock_str::msg_location_enabled(context).await);
        msg.param.set_cmd(SystemMessage::LocationStreamingEnabled);
        chat::send_msg(context, chat_id, &mut msg)
            .await
            .unwrap_or_default();
    } else if 0 == seconds && is_sending_locations_before {
        let stock_str = stock_str::msg_location_disabled(context).await;
        chat::add_info_msg(context, chat_id, &stock_str, now).await?;
    }
    context.emit_event(EventType::ChatModified(chat_id));
    if 0 != seconds {
        context.interrupt_location().await;
    }
    Ok(())
}

/// Returns whether `chat_id` or any chat is sending locations.
///
/// If `chat_id` is `Some` only that chat is checked, otherwise returns `true` if any chat
/// is sending locations.
pub async fn is_sending_locations_to_chat(
    context: &Context,
    chat_id: Option<ChatId>,
) -> Result<bool> {
    let exists = match chat_id {
        Some(chat_id) => {
            context
                .sql
                .exists(
                    "SELECT COUNT(id) FROM chats  WHERE id=?  AND locations_send_until>?;",
                    paramsv![chat_id, time()],
                )
                .await?
        }
        None => {
            context
                .sql
                .exists(
                    "SELECT COUNT(id) FROM chats  WHERE locations_send_until>?;",
                    paramsv![time()],
                )
                .await?
        }
    };
    Ok(exists)
}

pub async fn set(context: &Context, latitude: f64, longitude: f64, accuracy: f64) -> bool {
    if latitude == 0.0 && longitude == 0.0 {
        return true;
    }
    let mut continue_streaming = false;

    if let Ok(chats) = context
        .sql
        .query_map(
            "SELECT id FROM chats WHERE locations_send_until>?;",
            paramsv![time()],
            |row| row.get::<_, i32>(0),
            |chats| {
                chats
                    .collect::<std::result::Result<Vec<_>, _>>()
                    .map_err(Into::into)
            },
        )
        .await
    {
        for chat_id in chats {
            if let Err(err) = context.sql.execute(
                    "INSERT INTO locations  \
                     (latitude, longitude, accuracy, timestamp, chat_id, from_id) VALUES (?,?,?,?,?,?);",
                    paramsv![
                        latitude,
                        longitude,
                        accuracy,
                        time(),
                        chat_id,
                        ContactId::SELF,
                    ]
            ).await {
                warn!(context, "failed to store location {:?}", err);
            } else {
                info!(context, "stored location for chat {}", chat_id);
                continue_streaming = true;
            }
        }
        if continue_streaming {
            context.emit_event(EventType::LocationChanged(Some(ContactId::SELF)));
        };
    }

    continue_streaming
}

pub async fn get_range(
    context: &Context,
    chat_id: Option<ChatId>,
    contact_id: Option<u32>,
    timestamp_from: i64,
    mut timestamp_to: i64,
) -> Result<Vec<Location>> {
    if timestamp_to == 0 {
        timestamp_to = time() + 10;
    }

    let (disable_chat_id, chat_id) = match chat_id {
        Some(chat_id) => (0, chat_id),
        None => (1, ChatId::new(0)), // this ChatId is unused
    };
    let (disable_contact_id, contact_id) = match contact_id {
        Some(contact_id) => (0, contact_id),
        None => (1, 0), // this contact_id is unused
    };
    let list = context
        .sql
        .query_map(
            "SELECT l.id, l.latitude, l.longitude, l.accuracy, l.timestamp, l.independent, \
             COALESCE(m.id, 0) AS msg_id, l.from_id, l.chat_id, COALESCE(m.txt, '') AS txt \
             FROM locations l  LEFT JOIN msgs m ON l.id=m.location_id  WHERE (? OR l.chat_id=?) \
             AND (? OR l.from_id=?) \
             AND (l.independent=1 OR (l.timestamp>=? AND l.timestamp<=?)) \
             ORDER BY l.timestamp DESC, l.id DESC, msg_id DESC;",
            paramsv![
                disable_chat_id,
                chat_id,
                disable_contact_id,
                contact_id as i32,
                timestamp_from,
                timestamp_to,
            ],
            |row| {
                let msg_id = row.get(6)?;
                let txt: String = row.get(9)?;
                let marker = if msg_id != 0 && is_marker(&txt) {
                    Some(txt)
                } else {
                    None
                };
                let loc = Location {
                    location_id: row.get(0)?,
                    latitude: row.get(1)?,
                    longitude: row.get(2)?,
                    accuracy: row.get(3)?,
                    timestamp: row.get(4)?,
                    independent: row.get(5)?,
                    msg_id,
                    contact_id: row.get(7)?,
                    chat_id: row.get(8)?,
                    marker,
                };
                Ok(loc)
            },
            |locations| {
                let mut ret = Vec::new();

                for location in locations {
                    ret.push(location?);
                }
                Ok(ret)
            },
        )
        .await?;
    Ok(list)
}

fn is_marker(txt: &str) -> bool {
    let mut chars = txt.chars();
    if let Some(c) = chars.next() {
        !c.is_whitespace() && chars.next().is_none()
    } else {
        false
    }
}

/// Deletes all locations from the database.
pub async fn delete_all(context: &Context) -> Result<()> {
    context
        .sql
        .execute("DELETE FROM locations;", paramsv![])
        .await?;
    context.emit_event(EventType::LocationChanged(None));
    Ok(())
}

pub async fn get_kml(context: &Context, chat_id: ChatId) -> Result<(String, u32)> {
    let mut last_added_location_id = 0;

    let self_addr = context.get_primary_self_addr().await?;

    let (locations_send_begin, locations_send_until, locations_last_sent) = context.sql.query_row(
        "SELECT locations_send_begin, locations_send_until, locations_last_sent  FROM chats  WHERE id=?;",
        paramsv![chat_id], |row| {
            let send_begin: i64 = row.get(0)?;
            let send_until: i64 = row.get(1)?;
            let last_sent: i64 = row.get(2)?;

            Ok((send_begin, send_until, last_sent))
        })
        .await?;

    let now = time();
    let mut location_count = 0;
    let mut ret = String::new();
    if locations_send_begin != 0 && now <= locations_send_until {
        ret += &format!(
            "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
            <kml xmlns=\"http://www.opengis.net/kml/2.2\">\n<Document addr=\"{}\">\n",
            self_addr,
        );

        context
            .sql
            .query_map(
                "SELECT id, latitude, longitude, accuracy, timestamp \
             FROM locations  WHERE from_id=? \
             AND timestamp>=? \
             AND (timestamp>=? OR \
                  timestamp=(SELECT MAX(timestamp) FROM locations WHERE from_id=?)) \
             AND independent=0 \
             GROUP BY timestamp \
             ORDER BY timestamp;",
                paramsv![
                    ContactId::SELF,
                    locations_send_begin,
                    locations_last_sent,
                    ContactId::SELF
                ],
                |row| {
                    let location_id: i32 = row.get(0)?;
                    let latitude: f64 = row.get(1)?;
                    let longitude: f64 = row.get(2)?;
                    let accuracy: f64 = row.get(3)?;
                    let timestamp = get_kml_timestamp(row.get(4)?);

                    Ok((location_id, latitude, longitude, accuracy, timestamp))
                },
                |rows| {
                    for row in rows {
                        let (location_id, latitude, longitude, accuracy, timestamp) = row?;
                        ret += &format!(
                            "<Placemark>\
                <Timestamp><when>{}</when></Timestamp>\
                <Point><coordinates accuracy=\"{}\">{},{}</coordinates></Point>\
                </Placemark>\n",
                            timestamp, accuracy, longitude, latitude
                        );
                        location_count += 1;
                        last_added_location_id = location_id as u32;
                    }
                    Ok(())
                },
            )
            .await?;
        ret += "</Document>\n</kml>";
    }

    ensure!(location_count > 0, "No locations processed");

    Ok((ret, last_added_location_id))
}

fn get_kml_timestamp(utc: i64) -> String {
    // Returns a string formatted as YYYY-MM-DDTHH:MM:SSZ. The trailing `Z` indicates UTC.
    chrono::NaiveDateTime::from_timestamp(utc, 0)
        .format("%Y-%m-%dT%H:%M:%SZ")
        .to_string()
}

pub fn get_message_kml(timestamp: i64, latitude: f64, longitude: f64) -> String {
    format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
         <kml xmlns=\"http://www.opengis.net/kml/2.2\">\n\
         <Document>\n\
         <Placemark>\
         <Timestamp><when>{}</when></Timestamp>\
         <Point><coordinates>{},{}</coordinates></Point>\
         </Placemark>\n\
         </Document>\n\
         </kml>",
        get_kml_timestamp(timestamp),
        longitude,
        latitude,
    )
}

pub async fn set_kml_sent_timestamp(
    context: &Context,
    chat_id: ChatId,
    timestamp: i64,
) -> Result<()> {
    context
        .sql
        .execute(
            "UPDATE chats SET locations_last_sent=? WHERE id=?;",
            paramsv![timestamp, chat_id],
        )
        .await?;
    Ok(())
}

pub async fn set_msg_location_id(context: &Context, msg_id: MsgId, location_id: u32) -> Result<()> {
    context
        .sql
        .execute(
            "UPDATE msgs SET location_id=? WHERE id=?;",
            paramsv![location_id, msg_id],
        )
        .await?;

    Ok(())
}

/// Saves given locations to the database.
///
/// Returns the database row ID of the location with the highest timestamp.
pub(crate) async fn save(
    context: &Context,
    chat_id: ChatId,
    contact_id: ContactId,
    locations: &[Location],
    independent: bool,
) -> Result<Option<u32>> {
    ensure!(!chat_id.is_special(), "Invalid chat id");

    let mut newest_timestamp = 0;
    let mut newest_location_id = None;

    let stmt_insert = "INSERT INTO locations\
             (timestamp, from_id, chat_id, latitude, longitude, accuracy, independent) \
             VALUES (?,?,?,?,?,?,?);";

    for location in locations {
        let &Location {
            timestamp,
            latitude,
            longitude,
            accuracy,
            ..
        } = location;

        let conn = context.sql.get_conn().await?;
        let mut stmt_test =
            conn.prepare_cached("SELECT id FROM locations WHERE timestamp=? AND from_id=?")?;
        let mut stmt_insert = conn.prepare_cached(stmt_insert)?;

        let exists = stmt_test.exists(paramsv![timestamp, contact_id])?;

        if independent || !exists {
            stmt_insert.execute(paramsv![
                timestamp,
                contact_id,
                chat_id,
                latitude,
                longitude,
                accuracy,
                independent,
            ])?;

            if timestamp > newest_timestamp {
                // okay to drop, as we use cached prepared statements
                drop(stmt_test);
                drop(stmt_insert);
                newest_timestamp = timestamp;
                newest_location_id = Some(u32::try_from(conn.last_insert_rowid())?);
            }
        }
    }

    Ok(newest_location_id)
}

pub(crate) async fn location_loop(context: &Context, interrupt_receiver: Receiver<()>) {
    loop {
        let next_event = match maybe_send_locations(context).await {
            Err(err) => {
                warn!(context, "maybe_send_locations failed: {}", err);
                Some(60) // Retry one minute later.
            }
            Ok(next_event) => next_event,
        };

        let duration = if let Some(next_event) = next_event {
            Duration::from_secs(next_event)
        } else {
            Duration::from_secs(86400)
        };

        info!(
            context,
            "Location loop is waiting for {} or interrupt",
            duration_to_str(duration)
        );
        timeout(duration, interrupt_receiver.recv()).await.ok();
    }
}

/// Returns number of seconds until the next time location streaming for some chat ends
/// automatically.
async fn maybe_send_locations(context: &Context) -> Result<Option<u64>> {
    let mut next_event: Option<u64> = None;

    let now = time();
    let rows = context
        .sql
        .query_map(
            "SELECT id, locations_send_begin, locations_send_until, locations_last_sent
             FROM chats
             WHERE locations_send_until>0",
            [],
            |row| {
                let chat_id: ChatId = row.get(0)?;
                let locations_send_begin: i64 = row.get(1)?;
                let locations_send_until: i64 = row.get(2)?;
                let locations_last_sent: i64 = row.get(3)?;
                Ok((
                    chat_id,
                    locations_send_begin,
                    locations_send_until,
                    locations_last_sent,
                ))
            },
            |rows| {
                rows.collect::<std::result::Result<Vec<_>, _>>()
                    .map_err(Into::into)
            },
        )
        .await
        .context("failed to query location streaming chats")?;

    for (chat_id, locations_send_begin, locations_send_until, locations_last_sent) in rows {
        if locations_send_begin > 0 && locations_send_until > now {
            let can_send = now > locations_last_sent + 60;
            let has_locations = context
                .sql
                .exists(
                    "SELECT COUNT(id) \
     FROM locations \
     WHERE from_id=? \
     AND timestamp>=? \
     AND timestamp>? \
     AND independent=0",
                    paramsv![ContactId::SELF, locations_send_begin, locations_last_sent,],
                )
                .await?;

            next_event = next_event
                .into_iter()
                .chain(u64::try_from(locations_send_until - now).into_iter())
                .min();

            if has_locations {
                if can_send {
                    // Send location-only message.
                    // Pending locations are attached automatically to every message,
                    // so also to this empty text message.
                    info!(
                        context,
                        "Chat {} has pending locations, sending them.", chat_id
                    );
                    let mut msg = Message::new(Viewtype::Text);
                    msg.hidden = true;
                    msg.param.set_cmd(SystemMessage::LocationOnly);
                    chat::send_msg(context, chat_id, &mut msg).await?;
                } else {
                    // Wait until pending locations can be sent.
                    info!(
                        context,
                        "Chat {} has pending locations, but they can't be sent yet.", chat_id
                    );
                    next_event = next_event
                        .into_iter()
                        .chain(u64::try_from(locations_last_sent + 61 - now).into_iter())
                        .min();
                }
            } else {
                info!(
                    context,
                    "Chat {} has location streaming enabled, but no pending locations.", chat_id
                );
            }
        } else {
            // Location streaming was either explicitly disabled (locations_send_begin = 0) or
            // locations_send_until is in the past.
            info!(
                context,
                "Disabling location streaming for chat {}.", chat_id
            );
            context
                .sql
                .execute(
                    "UPDATE chats \
                         SET locations_send_begin=0, locations_send_until=0 \
                         WHERE id=?",
                    paramsv![chat_id],
                )
                .await
                .context("failed to disable location streaming")?;

            let stock_str = stock_str::msg_location_disabled(context).await;
            chat::add_info_msg(context, chat_id, &stock_str, now).await?;
            context.emit_event(EventType::ChatModified(chat_id));
        }
    }

    Ok(next_event)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::indexing_slicing)]

    use super::*;
    use crate::dc_receive_imf::dc_receive_imf;
    use crate::test_utils::TestContext;

    #[async_std::test]
    async fn test_kml_parse() {
        let context = TestContext::new().await;

        let xml =
            b"<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<kml xmlns=\"http://www.opengis.net/kml/2.2\">\n<Document addr=\"user@example.org\">\n<Placemark><Timestamp><when>2019-03-06T21:09:57Z</when></Timestamp><Point><coordinates accuracy=\"32.000000\">9.423110,53.790302</coordinates></Point></Placemark>\n<PlaceMARK>\n<Timestamp><WHEN > \n\t2018-12-13T22:11:12Z\t</WHEN></Timestamp><Point><coordinates aCCuracy=\"2.500000\"> 19.423110 \t , \n 63.790302\n </coordinates></Point></PlaceMARK>\n</Document>\n</kml>";

        let kml = Kml::parse(&context.ctx, xml).expect("parsing failed");

        assert!(kml.addr.is_some());
        assert_eq!(kml.addr.as_ref().unwrap(), "user@example.org",);

        let locations_ref = &kml.locations;
        assert_eq!(locations_ref.len(), 2);

        assert!(locations_ref[0].latitude > 53.6f64);
        assert!(locations_ref[0].latitude < 53.8f64);
        assert!(locations_ref[0].longitude > 9.3f64);
        assert!(locations_ref[0].longitude < 9.5f64);
        assert!(locations_ref[0].accuracy > 31.9f64);
        assert!(locations_ref[0].accuracy < 32.1f64);
        assert_eq!(locations_ref[0].timestamp, 1551906597);

        assert!(locations_ref[1].latitude > 63.6f64);
        assert!(locations_ref[1].latitude < 63.8f64);
        assert!(locations_ref[1].longitude > 19.3f64);
        assert!(locations_ref[1].longitude < 19.5f64);
        assert!(locations_ref[1].accuracy > 2.4f64);
        assert!(locations_ref[1].accuracy < 2.6f64);
        assert_eq!(locations_ref[1].timestamp, 1544739072);
    }

    #[async_std::test]
    async fn test_get_message_kml() {
        let context = TestContext::new().await;
        let timestamp = 1598490000;

        let xml = get_message_kml(timestamp, 51.423723f64, 8.552556f64);
        let kml = Kml::parse(&context.ctx, xml.as_bytes()).expect("parsing failed");
        let locations_ref = &kml.locations;
        assert_eq!(locations_ref.len(), 1);

        assert!(locations_ref[0].latitude >= 51.423723f64);
        assert!(locations_ref[0].latitude < 51.423724f64);
        assert!(locations_ref[0].longitude >= 8.552556f64);
        assert!(locations_ref[0].longitude < 8.552557f64);
        assert!(locations_ref[0].accuracy.abs() < f64::EPSILON);
        assert_eq!(locations_ref[0].timestamp, timestamp);
    }

    #[test]
    fn test_is_marker() {
        assert!(is_marker("f"));
        assert!(!is_marker("foo"));
        assert!(is_marker("🏠"));
        assert!(!is_marker(" "));
        assert!(!is_marker("\t"));
    }

    /// Tests that location.kml is hidden.
    #[async_std::test]
    async fn receive_location_kml() -> Result<()> {
        let alice = TestContext::new_alice().await;

        dc_receive_imf(
            &alice,
            br#"Subject: Hello
Message-ID: hello@example.net
To: Alice <alice@example.org>
From: Bob <bob@example.net>
Date: Mon, 20 Dec 2021 00:00:00 +0000
Chat-Version: 1.0
Content-Type: text/plain; charset=utf-8; format=flowed; delsp=no

Text message."#,
            false,
        )
        .await?;
        let received_msg = alice.get_last_msg().await;
        assert_eq!(received_msg.text.unwrap(), "Text message.");

        dc_receive_imf(
            &alice,
            br#"Subject: locations
MIME-Version: 1.0
To: <alice@example.org>
From: <bob@example.net>
Date: Tue, 21 Dec 2021 00:00:00 +0000
Chat-Version: 1.0
Message-ID: <foobar@example.net>
Content-Type: multipart/mixed; boundary="U8BOG8qNXfB0GgLiQ3PKUjlvdIuLRF"


--U8BOG8qNXfB0GgLiQ3PKUjlvdIuLRF
Content-Type: text/plain; charset=utf-8; format=flowed; delsp=no



--U8BOG8qNXfB0GgLiQ3PKUjlvdIuLRF
Content-Type: application/vnd.google-earth.kml+xml
Content-Disposition: attachment; filename="location.kml"

<?xml version="1.0" encoding="UTF-8"?>
<kml xmlns="http://www.opengis.net/kml/2.2">
<Document addr="bob@example.net">
<Placemark><Timestamp><when>2021-11-21T00:00:00Z</when></Timestamp><Point><coordinates accuracy="1.0000000000000000">10.00000000000000,20.00000000000000</coordinates></Point></Placemark>
</Document>
</kml>

--U8BOG8qNXfB0GgLiQ3PKUjlvdIuLRF--"#,
            false,
        )
        .await?;

        // Received location message is not visible, last message stays the same.
        let received_msg2 = alice.get_last_msg().await;
        assert_eq!(received_msg2.id, received_msg.id);

        let locations = get_range(&alice, None, None, 0, 0).await?;
        assert_eq!(locations.len(), 1);
        Ok(())
    }
}

#![feature(proc_macro)]

extern crate chrono;
extern crate futures;
extern crate hyper;
#[macro_use]
extern crate log;
extern crate oauthcli;
extern crate serde;
#[macro_use]
extern crate serde_derive;
extern crate serde_json as json;
extern crate url;

#[macro_use]
pub mod messages;

mod util;

pub use hyper::method::Method;
pub use hyper::status::StatusCode;
pub use messages::StreamMessage;

use futures::{Async, Future, Poll, Stream};
use hyper::client::Client;
use hyper::header::{Headers, Authorization, UserAgent};
use messages::{FilterLevel, UserId};
use oauthcli::{OAuthAuthorizationHeader, OAuthAuthorizationHeaderBuilder, SignatureMethod};
use util::{Lines, Timeout};
use std::convert::From;
use std::error::Error as StdError;
use std::fmt::{self, Display, Formatter};
use std::io::{self, BufReader};
use std::time::{Duration, Instant};
use url::Url;
use url::form_urlencoded::{Serializer, Target};

#[derive(Clone, Debug, Eq, PartialEq, Hash)]
pub struct Token<'a>(pub &'a str, pub &'a str);

#[derive(Clone, Debug)]
pub struct TwitterStreamBuilder<'a> {
	method: Method,
	end_point: &'a str,
    consumer: Token<'a>,
    token: Token<'a>,

    client: Option<&'a Client>,
    timeout: Duration,
    user_agent: Option<&'a str>,

    // API parameters:
    // delimited: bool, // Can/need not be handled by `TwitterStream`.
    stall_warnings: bool,
    filter_level: FilterLevel,
    language: Option<&'a str>,
    follow: Option<&'a [UserId]>,
    track: Option<&'a str>,
    locations: Option<&'a [((f64, f64), (f64, f64))]>,
    count: Option<i32>,
    with: Option<With>,
    replies: bool,
    // stringify_friend_ids: bool,
}

string_enums! {
    #[derive(Clone, Debug)]
    pub enum With {
        User("user"),
        Following("following");
        Custom(_),
    }
}

pub struct TwitterStream {
    lines: Lines,
    timeout: Duration,
    timer: Timeout,
}

#[derive(Debug)]
pub enum Error {
    Url(url::ParseError),
    Hyper(hyper::Error),
    Http(StatusCode),
    Io(io::Error),
    TimedOut(u64),
}

pub type Result<T> = std::result::Result<T, Error>;

macro_rules! def_builder_setters {
    (
        $(pub fn $name:ident($typ:ty);)*
        $(option pub fn $op_name:ident($op_typ:ty);)*
    ) => {
        $(
            pub fn $name(&mut self, $name: $typ) -> &mut Self {
                self.$name = $name.into();
                self
            }
        )*
        $(
            pub fn $op_name<T: Into<Option<$op_typ>>>(&mut self, $op_name: T) -> &mut Self {
                self.$op_name = $op_name.into();
                self
            }
        )*
    };
}

impl<'a> TwitterStreamBuilder<'a> {
    pub fn filter(consumer: Token<'a>, token: Token<'a>) -> Self {
        TwitterStreamBuilder::custom(
            Method::Post, "https://stream.twitter.com/1.1/statuses/filter.json", consumer, token
        )
    }

    pub fn sample(consumer: Token<'a>, token: Token<'a>) -> Self {
        TwitterStreamBuilder::custom(
            Method::Get, "https://stream.twitter.com/1.1/statuses/sample.json", consumer, token
        )
    }

    pub fn firehose(consumer: Token<'a>, token: Token<'a>) -> Self {
        TwitterStreamBuilder::custom(
            Method::Get, "https://stream.twitter.com/1.1/statuses/firehose.json", consumer, token
        )
    }

    pub fn user(consumer: Token<'a>, token: Token<'a>) -> Self {
        TwitterStreamBuilder::custom(
            Method::Get, "https://userstream.twitter.com/1.1/user.json", consumer, token
        )
    }

    pub fn site(consumer: Token<'a>, token: Token<'a>) -> Self {
        TwitterStreamBuilder::custom(
            Method::Get, "https://sitestream.twitter.com/1.1/site.json", consumer, token
		)
    }

    pub fn custom(method: Method, end_point: &'a str, consumer: Token<'a>, token: Token<'a>) -> Self {
        TwitterStreamBuilder {
            method: method,
            end_point: end_point,
            consumer: consumer,
            token: token,

            client: None,
            timeout: Duration::from_secs(90),
            user_agent: None,

            stall_warnings: false,
            filter_level: FilterLevel::None,
            language: None,
            follow: None,
            track: None,
            locations: None,
            count: None,
            with: None,
            replies: false,
        }
    }

    def_builder_setters! {
        pub fn timeout(Duration);
        pub fn stall_warnings(bool);
        pub fn filter_level(FilterLevel);
        pub fn replies(bool);

        option pub fn client(&'a Client);
        option pub fn user_agent(&'a str);
        option pub fn language(&'a str);
        option pub fn follow(&'a [UserId]);
        option pub fn track(&'a str);
        option pub fn locations(&'a [((f64, f64), (f64, f64))]);
        option pub fn count(i32);
        option pub fn with(With);
    }

    pub fn login(&self) -> Result<TwitterStream> {
        let mut url = Url::parse(self.end_point)?;

        let mut headers = Headers::new();
        if let Some(ua) = self.user_agent {
            headers.set(UserAgent(ua.to_owned()));
        }

        // Holds a borrowed or owned value.
        enum Hold<'a, T: 'a> {
            Borrowed(&'a T),
            Owned(T),
        }

        impl<'a, T: 'a> std::ops::Deref for Hold<'a, T> {
            type Target = T;
            fn deref(&self) -> &T {
                match *self {
                    Hold::Borrowed(t) => t,
                    Hold::Owned(ref t) => t,
                }
            }
        }

        let client = self.client
            .map(Hold::Borrowed)
            .unwrap_or_else(|| Hold::Owned(Client::new()));

        let res = if Method::Post == self.method {
            headers.set(self.create_authorization_header(&url));
            let mut body = Serializer::new(String::new());
            self.append_query_pairs(&mut body);
            client
                .post(url)
                .headers(headers)
                .body(&body.finish())
                .send()?
        } else {
            self.append_query_pairs(&mut url.query_pairs_mut());
            headers.set(self.create_authorization_header(&url));
            client
                .request(self.method.clone(), url)
                .headers(headers)
                .send()?
        };

        match &res.status {
            &StatusCode::Ok => (),
            _ => return Err(res.status.into()),
        }

        Ok(TwitterStream {
            lines: util::lines(BufReader::new(res)),
            timeout: self.timeout,
            timer: Timeout::after(self.timeout),
        })
    }

    fn append_query_pairs<T: Target>(&self, pairs: &mut Serializer<T>) {
        if self.stall_warnings {
            pairs.append_pair("stall_warnings", "true");
        }
        if self.filter_level != FilterLevel::None {
            pairs.append_pair("filter_level", self.filter_level.as_ref());
        }
        if let Some(s) = self.language {
            pairs.append_pair("language", s);
        }
        if let Some(ids) = self.follow {
            let mut val = String::new();
            if let Some(id) = ids.first() {
                val = id.to_string();
            }
            for id in ids.into_iter().skip(1) {
                val.push(',');
                val.push_str(&id.to_string());
            }
            pairs.append_pair("follow", &val);
        }
        if let Some(s) = self.track {
            pairs.append_pair("track", s);
        }
        if let Some(locs) = self.locations {
            let mut val = String::new();
            macro_rules! push {
                ($coordinate:expr) => {{
                    val.push(',');
                    val.push_str(&$coordinate.to_string());
                }};
            }
            if let Some(&((lon1, lat1), (lon2, lat2))) = locs.first() {
                val = lon1.to_string();
                push!(lat1);
                push!(lon2);
                push!(lat2);
            }
            for &((lon1, lat1), (lon2, lat2)) in locs.into_iter().skip(1) {
                push!(lon1);
                push!(lat1);
                push!(lon2);
                push!(lat2);
            }
            pairs.append_pair("locations", &val);
        }
        if let Some(n) = self.count {
            pairs.append_pair("count", &n.to_string());
        }
        if let Some(ref w) = self.with {
            pairs.append_pair("with", w.as_ref());
        }
        if self.replies {
            pairs.append_pair("replies", "all");
        }
    }

    fn create_authorization_header(&self, url: &Url) -> Authorization<OAuthAuthorizationHeader> {
        let oauth = OAuthAuthorizationHeaderBuilder::new(
            self.method.as_ref(), &url, self.consumer.0, self.consumer.1, SignatureMethod::HmacSha1
        )
            .token(self.token.0, self.token.1)
            .finish_for_twitter();

        Authorization(oauth)
    }
}

macro_rules! def_stream_constructors {
    ($(pub fn $name:ident;)*) => {
        $(
            pub fn $name<'a>(consumer: Token<'a>, token: Token<'a>) -> Result<Self> {
                TwitterStreamBuilder::$name(consumer, token).login()
            }
        )*
    };
}

impl TwitterStream {
    def_stream_constructors! {
        pub fn filter;
        pub fn sample;
        pub fn firehose;
        pub fn user;
        pub fn site;
    }
}

impl Stream for TwitterStream {
    type Item = String;
    type Error = Error;

    fn poll(&mut self) -> Poll<Option<String>, Error> {
        use Async::*;

        trace!("TwitterStream::poll");

        loop {
            match self.lines.poll()? {
                Ready(line_opt) => {
                    match line_opt {
                        Some(line) => {
                            let now = Instant::now();
                            let mut timer = Timeout::after(self.timeout);
                            timer.park(now);
                            info!("duration since last message: {}", {
                                let elapsed = timer.when() - self.timer.when(); // = (now + timeout) - (last + timeout)
                                elapsed.as_secs() as f64 + elapsed.subsec_nanos() as f64 / 1_000_000_000f64
                            });
                            self.timer = timer;

                            if line.is_empty() {
                                debug!("blank line");
                            } else {
                                return Ok(Ready(Some(line)));
                            }
                        },
                        None => return Ok(None.into()),
                    }
                },
                NotReady => {
                    if let Ok(Ready(())) = self.timer.poll() {
                        return Err(Error::TimedOut(self.timeout.as_secs()));
                    } else {
                        debug!("polled before being ready");
                        return Ok(NotReady);
                    }
                },
            }
        }
    }
}

impl IntoIterator for TwitterStream {
    type Item = Result<String>;
    type IntoIter = futures::stream::Wait<Self>;

    fn into_iter(self) -> Self::IntoIter {
        self.wait()
    }
}

impl StdError for Error {
    fn description(&self) -> &str {
        use Error::*;

        match *self {
            Url(ref e) => e.description(),
            Hyper(ref e) => e.description(),
            Http(ref status) => status.canonical_reason().unwrap_or("<unknown status code>"),
            Io(ref e) => e.description(),
            TimedOut(_) => "timed out",
        }
    }

    fn cause(&self) -> Option<&StdError> {
        use Error::*;

        match *self {
            Url(ref e) => Some(e),
            Hyper(ref e) => Some(e),
            Http(_) => None,
            Io(ref e) => Some(e),
            TimedOut(_) => None,
        }
    }
}

impl Display for Error {
    fn fmt(&self, f: &mut Formatter) -> fmt::Result {
        use Error::*;

        match *self {
            Url(ref e) => Display::fmt(e, f),
            Hyper(ref e) => Display::fmt(e, f),
            Http(ref code) => Display::fmt(code, f),
            Io(ref e) => Display::fmt(e, f),
            TimedOut(timeout) => write!(f, "connection timed out after {} sec", timeout),
        }
    }
}

impl From<url::ParseError> for Error {
    fn from(e: url::ParseError) -> Self {
        Error::Url(e)
    }
}

impl From<hyper::Error> for Error {
    fn from(e: hyper::Error) -> Self {
        Error::Hyper(e)
    }
}

impl From<StatusCode> for Error {
    fn from(e: StatusCode) -> Self {
        Error::Http(e)
    }
}

impl From<io::Error> for Error {
    fn from(e: io::Error) -> Self {
        Error::Io(e)
    }
}

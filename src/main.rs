extern crate bbb_core;
extern crate futures;
extern crate hyper;
#[macro_use]
extern crate nom;
extern crate rand;
extern crate regex;
#[macro_use]
extern crate serde;
extern crate tokio_core;

use std::env;
use std::net::SocketAddr;
use std::sync::mpsc::{Receiver, Sender, channel};

use bbb_core::expr::Expr;
use bbb_core::parser::parse as parse_expr;
use bbb_core::signal::ExprSignal;
use bbb_core::wav::Recorder;
use futures::future;
use futures::{Future, Stream};
use hyper::Method::Post;
use hyper::StatusCode;
use hyper::mime::APPLICATION_JSON;
use hyper::server::{Http, Request, Response, Service};
use hyper::{Client, Uri};
use nom::{digit, float, rest};
use rand::Rng;
use regex::Regex;
use tokio_core::net::{TcpListener, TcpStream};
use tokio_core::reactor::{Core, Remote};
use tokio_core::reactor::Handle as CoreHandle;

fn main() {
    let port = env::var("PORT")
        .or::<Result<String, env::VarError>>(Ok("3000".to_owned()))
        .unwrap();
    let address = format!("0.0.0.0:{}", port).parse().expect(&*format!(
        "Cound not form URI address with port {}",
        port
    ));

    let accepted_tokens = env::var("VALID_TOKENS")
        .map(|s| s.split(':').map(String::from).collect())
        .or::<Result<Vec<String>, env::VarError>>(Ok(vec![]))
        .unwrap();

    let mut core = Core::new().unwrap();
    let handle = core.handle();
    let remote = core.remote();

    let (sender, receiver) = channel();
    let bbb = BBB::new(accepted_tokens, sender);
    let responder = Responder::new(remote, receiver);
    let server = prepare_server(&handle, address, bbb).unwrap();

    core.run(server).map_err(err_string);
    // match run_server(address, accepted_tokens) {
    //     Err(e) => println!("Server error: {}", e),
    //     _ => (),
    // }
}

fn err_string<E: std::error::Error>(e: E) -> String {
    e.description().to_owned()
}

fn prepare_server<F>(
    handle: &CoreHandle,
    address: std::net::SocketAddr,
    service: BBB,
) -> Result<F, String>
where
    F: Future<Item = (), Error = std::io::Error>,
{
    let listener = TcpListener::bind(&address, &handle).map_err(err_string)?;
    let http = Http::new();

    // use futures::IntoFuture;
    let server = listener.incoming().for_each(|(socket, addr)| {
        http.bind_connection(&handle, socket, addr, service);
        Ok(())
    });

    Ok(server)
}

struct BBB {
    valid_tokens: Vec<String>,
    channel: Sender<ExprOrder>,
}

impl BBB {
    fn new(tokens: Vec<String>, channel: Sender<ExprOrder>) -> Self {
        BBB {
            valid_tokens: tokens,
            channel: channel,
        }
    }

    fn handle(&self, body: hyper::Body) -> Box<Future<Item = Response, Error = hyper::Error>> {
        Box::new(
            unchunk(body).then(move |body_string| {
                let validation = body_string.map_err(err_string).and_then(|bs| {
                    validate_request(&self.valid_tokens, bs)
                });

                match validation {
                    Err(_) => respond_forbidden(),
                    Ok(body_string) => {
                        match parse(body_string).and_then(|o| self.place_order(o)) {
                            Err(_) => respond_unprocessable(),
                            Ok(file_name) => respond_successfully("singing to myself".to_owned()),
                        }
                    }
                }
            })
        )
    }

    fn place_order(&self, order: ExprOrder) -> Result<(), String> {
        self.channel.send(order).map_err(err_string)
    }
}

impl Service for BBB {
    type Request = Request;
    type Response = Response;
    type Error = hyper::Error;
    type Future = Box<Future<Item = Self::Response, Error = Self::Error>>;

    fn call(&self, req: Request) -> Self::Future {
        let method = req.method().clone();

        match method {
            Post => Box::new(self.handle(req.body())),
            _ => Box::new(future::ok(
                Response::new().with_status(StatusCode::NotFound),
            )),
        }
    }
}

struct Responder {
    handle: Remote,
    channel: Receiver<ExprOrder>,
}

impl Responder {
    fn new(handle: Remote, channel: Receiver<ExprOrder>) -> Self {
        Responder {
            handle: handle,
            channel: channel,
        }
    }

    fn run(&self) -> Result<(), String> {
        loop {
            match self.channel.recv() {
                Ok(order) => {

                },
                _ => (),
            }
        }
    }
}

struct ExprSpec {
    expr: Expr,
    duration: f32,
}

struct ExprOrder {
    spec: ExprSpec,
    destination: Uri,
}

type StringFuture = Box<Future<Item = String, Error = hyper::Error>>;

fn unchunk(body: hyper::Body) -> StringFuture {
    Box::new(
        body.fold(Vec::new(), |mut acc, chunk| {
            acc.extend_from_slice(&*chunk);
            futures::future::ok::<_, hyper::Error>(acc)
        }).map(|chunks| String::from_utf8(chunks).unwrap_or(String::new())),
    )
}

fn validate_request(tokens: &Vec<String>, body: String) -> Result<String, String> {
    let re = Regex::new(r"token=(\w+\)&").expect("error compiling token regex");
    let check = re.captures(&*body)
        .and_then(|cap| cap.get(1))
        .map(|mtch| {
            tokens.iter().any(|tok| tok.as_str() == mtch.as_str())
        })
        .unwrap_or(false);

    if check {
        Ok(body)
    } else {
        Err(String::from("Token is invalid"))
    }
}

fn parse(body: String) -> Result<ExprOrder, String> {
    let spec = get_spec(&*body);
    let destination = get_destination(&*body);

    match (spec, destination) {
        (Ok(spec), Ok(destination)) => Ok(ExprOrder { spec: spec, destination: destination }),
        _ => Err("Malformed request".to_owned()),
    }
}

fn get_spec(body: &str) -> Result<ExprSpec, String> {
    let re = Regex::new(r"text=(.+)").expect("error compiling response url regex");
    match re.captures(body).and_then(|cap| cap.get(1)) {
        Some(m) => order(m.as_str().as_bytes()).to_result().or(Err("Invalid expression".to_owned())),
        None => Err("No response url found".to_owned()),
    }
}

fn get_destination(body: &str) -> Result<Uri, String> {
    let re = Regex::new(r"response_url=(https://[a-z0-9:/.]+)").expect("error compiling response url regex");
    use std::str::FromStr;
    match re.captures(body).and_then(|cap| cap.get(1)) {
        Some(m) => Uri::from_str(m.as_str()).map_err(err_string),
        None => Err("No response url found".to_owned()),
    }
}

named!(order<ExprSpec>, ws!(map!(
    pair!(
        duration,
        map_res!(
            call!(rest),
            |bytes| std::str::from_utf8(bytes).map_err(err_string).and_then(parse_expr)
        )
    ),
    |(d, e): (Option<f32>, Expr)| ExprSpec { expr: e, duration: d.unwrap_or(10f32) }
)));

enum DurUnit { Second, Minute }

named!(duration<Option<f32>>, ws!(opt!(
    map!(
        pair!(duration_amount, duration_unit),
        |(amt, unit)| {
            match unit {
                DurUnit::Second => amt,
                DurUnit::Minute => amt * 60f32,
            }
        }
    )
)));

named!(duration_amount<f32>, alt!(
    call!(float) |
    map_res!(
        call!(digit),
        |d| std::str::from_utf8(d)
            .map_err(err_string)
            .and_then(|s| s.parse::<f32>().map_err(err_string))
    )
));

named!(duration_unit<DurUnit>, alt!(seconds | minutes));
named!(seconds<DurUnit>, value!(DurUnit::Second, alt!(tag!("s") | tag!("sec") | tag!("\""))));
named!(minutes<DurUnit>, value!(DurUnit::Minute, alt!(tag!("m") | tag!("min") | tag!("'"))));

fn perform(order: ExprSpec) -> Result<String, String> {
    let tmp_file: String = rand::thread_rng()
        .gen_ascii_chars()
        .take(20)
        .collect::<String>() + ".wav";

    let result = Recorder::new(8_000 as u32).record(
        &*tmp_file,
        order.duration,
        &mut ExprSignal::from(order.expr),
    );

    match result {
        Ok(_) => Ok(tmp_file),
        Err(e) => Ok(e),
    }
}

type ResponseFuture = Box<Future<Item = Response, Error = hyper::Error>>;

fn respond_successfully(wav_file: String) -> ResponseFuture {
    Box::new(future::ok(Response::new().with_status(StatusCode::Ok)))
}

fn respond_unprocessable() -> ResponseFuture {
    Box::new(future::ok(
        Response::new().with_status(StatusCode::UnprocessableEntity),
    ))
}

fn respond_forbidden() -> ResponseFuture {
    Box::new(future::ok(
        Response::new().with_status(StatusCode::Forbidden),
    ))
}

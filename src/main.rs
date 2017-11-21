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

use bbb_core::expr::Expr;
use bbb_core::parser::parse as parse_expr;
use bbb_core::signal::ExprSignal;
use bbb_core::wav::Recorder;
use futures::future;
use futures::{Future, Stream};
use hyper::Method::{Post};
use hyper::StatusCode;
use hyper::mime::APPLICATION_JSON;
use hyper::server::{Http, Request, Response, Service};
use hyper::{Client, Uri};
use nom::{digit, float, rest};
use rand::Rng;
use regex::Regex;
use tokio_core::net::TcpListener;
use tokio_core::reactor::Core;

fn main() {
    let port = env::var("PORT").or::<Result<String, env::VarError>>(Ok("3000".to_owned())).unwrap();
    let address = format!("0.0.0.0:{}", port).parse().expect(
        &*format!("Cound not form URI address with port {}", port)
    );

    let accepted_tokens = env::var("VALID_TOKENS")
        .map(|s| s.split(':').map(String::from).collect())
        .or::<Result<Vec<String>, env::VarError>>(Ok(vec![]))
        .unwrap();

    match run_server(address, accepted_tokens) {
        Err(e) => println!("Server error: {}", e),
        _ => (),
    }
}

fn err_string<E: std::error::Error>(e: E) -> String {
    e.description().to_owned()
}

fn run_server(address: std::net::SocketAddr, tokens: Vec<String>) -> Result<(), String> {
    let mut core = Core::new().map_err(err_string)?;
    let handle = core.handle();
    let listener = TcpListener::bind(&address, &handle).map_err(err_string)?;
    let http = Http::new();
    let server = listener.incoming().for_each(|(socket, addr)| {
        let bbb_handle = handle.clone();
        let tokens = tokens.clone();
        let bbb = BBB { valid_tokens: tokens, handle: bbb_handle, };
        http.bind_connection(&handle, socket, addr, bbb);
        Ok(())
    });

    core.run(server).map_err(err_string)
}

struct BBB {
    valid_tokens: Vec<String>,
    handle: tokio_core::reactor::Handle,
}

impl Service for BBB {
    type Request = Request;
    type Response = Response;
    type Error = hyper::Error;
    type Future = Box<Future<Item = Self::Response, Error = Self::Error>>;

    fn call(&self, req: Request) -> Self::Future {
        let valid_tokens = self.valid_tokens.clone();
        let method = req.method().clone();

        match method {
            Post => Box::new(
                unchunk(req.body()).then(move |body_string| {
                    let validation = body_string
                        .map_err(err_string)
                        .and_then(|bs| validate_request(&valid_tokens, bs));

                    match validation {
                        Err(_) => respond_forbidden(),
                        Ok(body_string) => match parse(body_string).and_then(perform) {
                            Err(_) => respond_unprocessable(),
                            Ok(file_name) => respond_successfully(file_name, self.handle.clone()),
                        }
                    }
                })
            ),
            _ => Box::new(future::ok(
                Response::new().with_status(StatusCode::NotFound)
            )),
        }
    }
}

struct ExprOrder {
    expr: Expr,
    duration: f32,
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
    let check = re.captures(&*body).and_then(|cap| cap.get(1)).map(|mtch| {
        tokens.iter().any(|tok| tok.as_str() == mtch.as_str())
    }).unwrap_or(false);

    if check {
        Ok(body)
    } else {
        Err(String::from("Token is invalid"))
    }
}

named!(order<ExprOrder>, ws!(map!(
    pair!(
        duration,
        map_res!(
            call!(rest),
            |bytes| std::str::from_utf8(bytes).map_err(err_string).and_then(parse_expr)
        )
    ),
    |(d, e): (Option<f32>, Expr)| ExprOrder { expr: e, duration: d.unwrap_or(10f32) }
)));

named!(duration<Option<f32>>, ws!(opt!(
    alt!(
        call!(float) |
        map_opt!(
            call!(digit),
            |d| std::str::from_utf8(d).ok().and_then(|s| s.parse::<f32>().ok().or(None))
        )
    )
)));

fn parse(body: String) -> Result<ExprOrder, String> {
    use nom::IResult::{Done, Incomplete, Error};
    match order(body.as_bytes()) {
        Done(_, order) => Ok(order),
        Incomplete(_) => Err(format!("could not complete parsing: {}", body)),
        Error(_) => Err(format!("error when parsing: {}", body)),
    }
}

fn perform(order: ExprOrder) -> Result<String, String> {
    let tmp_file: String = rand::thread_rng()
        .gen_ascii_chars()
        .take(20)
        .collect::<String>() + ".wav";

    let result = Recorder::new(8_000 as u32).record(
        &*tmp_file,
        order.duration,
        &mut ExprSignal::from(order.expr)
    );

    match result {
        Ok(_) => Ok(tmp_file),
        Err(e) => Ok(e),
    }
}

type ResponseFuture = Box<Future<Item = Response, Error = hyper::Error>>;

fn respond_successfully(wav_file: String, handle: tokio_core::reactor::Handle) -> ResponseFuture {
    Box::new(future::ok(
        Response::new().with_status(StatusCode::Ok)
    ))
}

fn respond_unprocessable() -> ResponseFuture {
    Box::new(future::ok(
        Response::new().with_status(StatusCode::UnprocessableEntity)
    ))
}

fn respond_forbidden() -> ResponseFuture {
    Box::new(future::ok(
        Response::new().with_status(StatusCode::Forbidden)
    ))
}

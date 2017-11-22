extern crate bbb_core;
extern crate futures;
extern crate hyper;
#[macro_use]
extern crate nom;
extern crate rand;
extern crate regex;
extern crate serde;
#[macro_use]
extern crate serde_derive;
extern crate serde_json;
extern crate serde_urlencoded as serde_form;
extern crate tokio_core;
extern crate url;
extern crate url_serde;

use std::env;
use std::fs::File;
use std::io::Read;
use std::net::SocketAddr;
use std::sync::mpsc::{Receiver, Sender, channel};
use std::thread::spawn;

use bbb_core::expr::Expr;
use bbb_core::parser::parse as parse_expr;
use bbb_core::signal::ExprSignal;
use bbb_core::wav::Recorder;
use futures::future;
use futures::{Future, Stream};
use hyper::Method::Post;
use hyper::StatusCode;
use hyper::header::ContentType;
use hyper::mime::Mime;
use hyper::mime::{MULTIPART_FORM_DATA, APPLICATION_JSON};
use hyper::server::{Http, Request, Response, Service};
use hyper::Client;
use nom::{digit, float, rest};
use rand::Rng;
use tokio_core::net::TcpListener;
use tokio_core::reactor::{Core, Remote, Handle as CoreHandle};
use url::Url;

fn main() {
    let token = env::var("SLACK_TOKEN").unwrap();
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

    println!("listening on {}, using tokens {:?}", address, accepted_tokens);

    let mut core = Core::new().unwrap();
    let handle = core.handle();
    let remote = core.remote();

    let (sender, receiver) = channel();
    let bbb = BBB::new(accepted_tokens, sender);
    let responder = Responder::new(remote, receiver, token);
    let server = prepare_server(handle, address, bbb);

    spawn(move || responder.run());
    core.run(server).map_err(err_string).unwrap();
}

fn prepare_server(
    handle: CoreHandle,
    address: SocketAddr,
    bbb: BBB,
) -> Box<Future<Item = (), Error = std::io::Error>> {
    let listener = TcpListener::bind(&address, &handle)
        .map_err(err_string)
        .expect("could not create tcp listener");

    use futures::IntoFuture;
    Box::new(
        listener
            .incoming()
            .for_each(move |(socket, addr)| {
                let http = Http::new();
                http.bind_connection(&handle, socket, addr, bbb.clone());
                Ok(())
            })
            .into_future(),
    )
}

fn err_string<E: std::error::Error>(e: E) -> String {
    e.description().to_owned()
}

#[derive(Clone)]
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
        let responder = self.channel.clone();
        let tokens = self.valid_tokens.clone();

        Box::new(parse_order(body).and_then(move |order| {
            let validation = order.and_then(|o| validate_request(&tokens, o));

            match validation {
                Err(_) => respond_forbidden(),
                Ok(order) => {
                    match parse(order) {
                        Err(_) => respond_unprocessable(),
                        Ok(order) => {
                            responder.send(order).ok();
                            respond_successfully("singing to myself...".to_owned())
                        }
                    }
                }
            }
        }))
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
    token: String,
}

impl Responder {
    fn new(handle: Remote, channel: Receiver<ExprOrder>, token: String) -> Self {
        Responder {
            handle: handle,
            channel: channel,
            token: token,
        }
    }

    fn run(&self) -> Result<(), String> {
        loop {
            match self.channel.recv() {
                Err(_) => continue,
                Ok(expr_order) => {
                    match perform(expr_order.spec) {
                        Err(_) => {
                            self.notify_error(
                                expr_order.order.response_url,
                                "I've lost my voice unforunately...".to_owned(),
                            )
                        }
                        Ok(file_name) => {
                            let upload_url = Url::parse("https://slack.com/api/files.upload")
                                .unwrap();
                            self.upload_file(upload_url, expr_order.order, file_name)
                        }
                    }
                }
            }
        }
    }

    fn notify_error(&self, url: Url, error_message: String) {
        self.handle.spawn(|handle| {
            let client = Client::new(handle);
            let mut request = Request::new(Post, url.into_string().parse().unwrap());

            set_content_type(&mut request, APPLICATION_JSON);
            request.set_body(
                serde_json::to_string(&ErrorMessage::new(error_message)).unwrap(),
            );

            client.request(request).then(|_| Ok(()))
        });
    }

    fn upload_file(&self, url: Url, order: Order, file_path: String) {
        let token = self.token.clone();

        self.handle.spawn(move |handle| {
            let client = Client::new(handle);
            let mut request = Request::new(Post, url.into_string().parse().unwrap());
            let mut file = File::open(&*file_path).unwrap();
            let mut contents = Vec::new();
            file.read_to_end(&mut contents).unwrap();

            set_content_type(&mut request, MULTIPART_FORM_DATA);
            request.set_body(
                serde_form::to_string(&FileUploadForm {
                    token: token,
                    channels: vec![order.channel_name],
                    filename: file_path.clone(),
                    filetype: "wav".to_owned(),
                    title: file_path,
                    initial_comment: format!("a beautiful song for {}", order.user_name),
                    file: contents,
                }).unwrap_or("".to_owned()),
            );

            client.request(request).then(|_| Ok(()))
        });
    }
}

fn set_content_type(request: &mut Request, content_type: Mime) {
    let headers = request.headers_mut();
    headers.set(ContentType(content_type));
}

#[derive(Serialize, Deserialize)]
struct FileUploadForm {
    token: String,
    channels: Vec<String>,
    filename: String,
    filetype: String,
    title: String,
    initial_comment: String,
    file: Vec<u8>,
}

#[derive(Serialize, Deserialize, Debug)]
struct Order {
    token: String,
    team_id: String,
    team_domain: String,
    enterprise_id: String,
    enterprise_name: String,
    channel_id: String,
    channel_name: String,
    user_id: String,
    user_name: String,
    command: String,
    text: String,
    #[serde(with = "url_serde")]
    response_url: Url,
    trigger_id: String,
}

#[derive(Serialize, Deserialize)]
struct ErrorMessage {
    text: String,
}

impl ErrorMessage {
    fn new(message: String) -> Self {
        ErrorMessage { text: message }
    }
}

struct ExprSpec {
    expr: Expr,
    duration: f32,
}

struct ExprOrder {
    spec: ExprSpec,
    order: Order,
}

type OrderFuture = Box<Future<Item = Result<Order, String>, Error = hyper::Error>>;

fn parse_order(body: hyper::Body) -> OrderFuture {
    Box::new(
        body.fold(Vec::new(), |mut acc, chunk| {
            acc.extend_from_slice(&*chunk);
            futures::future::ok::<_, hyper::Error>(acc)
        }).map(|chunks| String::from_utf8(chunks).unwrap_or(String::new()))
            .map(|string| serde_form::from_str(&*string).map_err(err_string)),
    )
}

fn validate_request(tokens: &Vec<String>, order: Order) -> Result<Order, String> {
    if tokens.iter().any(|t| *t == order.token) {
        Ok(order)
    } else {
        Err(String::from("Token is invalid"))
    }
}

fn parse(order: Order) -> Result<ExprOrder, String> {
    extract_spec(&order).map(|spec| ExprOrder {
        spec: spec,
        order: order,
    })
}

fn extract_spec(order: &Order) -> Result<ExprSpec, String> {
    spec(order.text.clone().as_bytes()).to_result().or(Err(
        "Could not parse expression".to_owned(),
    ))
}

named!(spec<ExprSpec>, complete!(ws!(map!(
    pair!(
        duration,
        map_res!(
            call!(rest),
            |bytes| std::str::from_utf8(bytes).map_err(err_string).and_then(parse_expr)
        )
    ),
    |(d, e): (Option<f32>, Expr)| ExprSpec { expr: e, duration: d.unwrap_or(10f32) }
))));

enum DurUnit {
    Second,
    Minute,
}

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
    let tmp_file_id: String = rand::thread_rng()
        .gen_ascii_chars()
        .take(10)
        .collect::<String>();

    let tmp_file = format!("/var/tmp/beats_{}.wav", tmp_file_id);

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

#[derive(Serialize, Deserialize)]
struct SuccessMessage {
    text: String,
}

fn respond_successfully(message: String) -> ResponseFuture {
    let message = SuccessMessage { text: message };
    Box::new(future::ok(
        Response::new()
            .with_status(StatusCode::Ok)
            .with_body(serde_json::to_string(&message).unwrap())
            .with_header(ContentType(APPLICATION_JSON))
    ))
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

use anyhow::{anyhow, bail, Result};
use dropshot::{
    endpoint, ApiDescription, ConfigDropshot, ConfigLogging,
    ConfigLoggingLevel, HttpError, HttpServerStarter, RequestContext,
};
use ec2::{
    DescribeInstancesRequest, DescribeInstancesResult, Ec2, Ec2Client, Tag,
};
use getopts::Options;
use hyper::http::header;
use hyper::{Body, Response};
use rusoto_core::{HttpClient, Region};
use rusoto_credential::StaticProvider;
use rusoto_ec2 as ec2;
use serde::Deserialize;
use slog::{crit, error, info, Logger};
use std::io::Read;
use std::process::exit;
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;
use tokio::try_join;

#[derive(Deserialize, Clone)]
struct ConfigAWS {
    access_key_id: String,
    secret_access_key: String,
}

#[derive(Deserialize, Clone)]
struct Config {
    refresh_interval: u64,
    int_suffix: String,
    shared_secret: String,
    aws: ConfigAWS,
}

struct App {
    cache: Arc<Cache>,
    config: Config,
}

#[derive(Debug, Clone)]
struct Host {
    id: String,
    name: Option<String>,
    ipv4: String,
}

struct CacheInner {
    hosts: Vec<Host>,
}

struct Cache {
    config: Config,
    inner: Mutex<Option<CacheInner>>,
    cv: Condvar,
}

trait TagExtractor {
    fn tag(&self, n: &str) -> Option<String>;
}

impl TagExtractor for Option<Vec<Tag>> {
    fn tag(&self, n: &str) -> Option<String> {
        if let Some(tags) = self.as_ref() {
            for tag in tags.iter() {
                if let Some(k) = tag.key.as_deref() {
                    if k == n {
                        return tag.value.clone();
                    }
                }
            }
        }

        None
    }
}

fn extract_hosts(res: &DescribeInstancesResult) -> Result<Vec<Host>> {
    let mut hosts = Vec::new();

    let reserv = res.reservations.as_ref().ok_or(anyhow!("no reservations"))?;

    for r in reserv {
        if let Some(i) = &r.instances {
            for i in i.iter() {
                let id = i.instance_id.clone().unwrap();
                let ipv4 = if let Some(ip) = i.private_ip_address.as_deref() {
                    ip.to_string()
                } else {
                    /*
                     * If there is not a private IP address for this instance,
                     * we just won't include it in the hosts file.
                     */
                    continue;
                };
                let name = i.tags.tag("Name");

                hosts.push(Host { id, name, ipv4 });
            }
        }
    }

    Ok(hosts)
}

async fn cache_update(log: Logger, cache: Arc<Cache>) -> Result<()> {
    let credprov = StaticProvider::new_minimal(
        cache.config.aws.access_key_id.clone(),
        cache.config.aws.secret_access_key.clone(),
    );
    let ec2 =
        Ec2Client::new_with(HttpClient::new()?, credprov, Region::UsWest2);

    let delay = Duration::from_secs(cache.config.refresh_interval);

    info!(log, "start cache update task");
    loop {
        let res = ec2
            .describe_instances(DescribeInstancesRequest {
                ..Default::default()
            })
            .await;

        let hosts = match res {
            Ok(res) => match extract_hosts(&res) {
                Ok(hosts) => hosts,
                Err(e) => {
                    error!(log, "extract_hosts: {:?}", e);
                    tokio::time::sleep(delay).await;
                    continue;
                }
            },
            Err(e) => {
                error!(log, "describe instances failure: {:?}", e);
                tokio::time::sleep(delay).await;
                continue;
            }
        };

        {
            let mut inner = cache.inner.lock().unwrap();
            *inner = Some(CacheInner { hosts });
            cache.cv.notify_all();
        }

        tokio::time::sleep(delay).await;
    }
}

#[endpoint {
    method = GET,
    path = "/hosts",
}]
async fn handle_hosts(
    rqctx: Arc<RequestContext<App>>,
) -> std::result::Result<Response<Body>, HttpError> {
    let app = rqctx.context();

    let auth = {
        let req = rqctx.request.lock().await;
        if let Some(h) = req.headers().get(header::AUTHORIZATION) {
            if let Ok(v) = h.to_str() {
                Some(v.to_string())
            } else {
                None
            }
        } else {
            None
        }
    };

    let ok = if let Some(auth) = auth.as_deref() {
        auth == &app.config.shared_secret
    } else {
        false
    };

    if !ok {
        return Err(HttpError::for_client_error(
            None,
            hyper::StatusCode::UNAUTHORIZED,
            "invalid".into(),
        ));
    }

    /*
     * Check for the expected shared secret first.
     */

    let hosts = {
        let inner = app.cache.inner.lock().unwrap();
        if let Some(inner) = &*inner {
            Some(inner.hosts.clone())
        } else {
            None
        }
    };

    if let Some(hosts) = &hosts {
        let mut out = String::new();
        out += &format!("{:16} {}\n", "127.0.0.1", "localhost loghost");
        out += &format!("{:16} {}\n", "::1", "localhost");
        out += "\n";
        for host in hosts.iter() {
            let mut names = Vec::new();
            if let Some(n) = host.name.as_deref() {
                names.push(n.to_string());
                if !app.config.int_suffix.is_empty() {
                    names.push(format!(
                        "{}{}",
                        n.to_string(),
                        app.config.int_suffix
                    ));
                }
            }
            names.push(host.id.to_string());

            out += &format!("{:16} {}\n", host.ipv4, names.join(" "));
        }

        Ok(Response::builder()
            .status(200)
            .header("content-type", "text/plain")
            .body(Body::from(out))?)
    } else {
        Err(HttpError::for_internal_error("host list not yet available".into()))
    }
}

fn read_config(f: &str) -> Result<Config> {
    let mut buf: Vec<u8> = Vec::new();
    let mut f = std::fs::File::open(&f)?;
    f.read_to_end(&mut buf)?;
    Ok(toml::from_slice(&buf)?)
}

#[tokio::main]
async fn main() -> Result<()> {
    let mut opts = Options::new();

    opts.reqopt("b", "", "bind address:port", "BIND_ADDRESS");
    opts.reqopt("f", "", "configuration file", "CONFIG");

    let p = match opts.parse(std::env::args().skip(1)) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("ERROR: usage: {}", e);
            eprintln!("       {}", opts.usage("usage"));
            exit(1);
        }
    };
    let bind = p.opt_str("b").unwrap_or(String::from("0.0.0.0:9977"));

    let cfglog =
        ConfigLogging::StderrTerminal { level: ConfigLoggingLevel::Info };
    let log = cfglog.to_logger("clag")?;

    let config = read_config(&p.opt_str("f").unwrap())?;

    let cache = Arc::new(Cache {
        inner: Mutex::new(None),
        cv: Condvar::new(),
        config: config.clone(),
    });

    /*
     * Kick off background task to update AWS instance list.
     */
    let cache0 = Arc::clone(&cache);
    let log0 = log.clone();
    let t_update = tokio::task::spawn(async move {
        cache_update(log0, cache0)
            .await
            .map_err(|e| anyhow!("cache update task failure: {:?}", e))
    });

    /*
     * Start API server.
     */
    let app = App { cache, config };

    let mut api = ApiDescription::new();
    api.register(handle_hosts).unwrap();

    let cfgds =
        ConfigDropshot { bind_address: bind.parse()?, ..Default::default() };

    let log0 = log.clone();
    let t_server = tokio::task::spawn(async move {
        HttpServerStarter::new(&cfgds, api, app, &log0)
            .map_err(|e| anyhow!("server task failure: {:?}", e))
            .map(|s| s.start())
    });

    /*
     * All tasks that we start should persist forever; if one exits, whether
     * fatally or not, it is an error and we need to bail out.
     */
    let res = try_join!(t_update, t_server);
    match res {
        Ok(_) => crit!(log, "tasks exited unexpectedly"),
        Err(e) => crit!(log, "task failure: {:?}", e),
    }
    bail!("early exit not expected");
}

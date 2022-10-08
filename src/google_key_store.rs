use actix::Context;
use actix_web::http::header;
use anyhow::Context as OtherContext;
use awc::{Client, SendClientRequest};
use regex::Regex;
use serde::Deserialize;
use std::collections::HashMap;
use std::time::Duration;

use actix::{prelude::*, Actor};

use jsonwebtoken::DecodingKey;

use anyhow::{anyhow, bail, Result};
use tracing::log::{debug, error, info};

type KeyMap = HashMap<String, DecodingKey>;

pub struct GoogleKeyStore {
    pub client: Client,
    pub keys: KeyMap,
}

impl GoogleKeyStore {
    pub fn start() -> Addr<Self> {
        GoogleKeyStore::create(|_ctx| GoogleKeyStore {
            client: Client::new(),
            keys: HashMap::new(),
        })
    }
}

impl Actor for GoogleKeyStore {
    type Context = Context<Self>;
    fn started(&mut self, ctx: &mut Context<Self>) {
        ctx.address().do_send(RefreshCache {});
    }
}

#[derive(Debug, Deserialize)]
struct Key {
    e: String,
    n: String,
    // r#use: String,
    // kty: String,
    kid: String,
    // alg: String,
}

#[derive(Debug, Deserialize)]
struct KeyData {
    keys: Vec<Key>,
}

async fn refresh_token(res: SendClientRequest) -> Result<(KeyMap, Duration)> {
    let mut response = match res.await {
        Ok(r) => r,
        Err(e) => bail!("{:?}", e), // I don't understand why I can't propagate using '?'
    };
    let cc = response
        .headers()
        .get(header::CACHE_CONTROL)
        .context("Could not find cache control header")?;
    let re = Regex::new(r"max-age=(\d*),")?;
    let cap = re
        .captures(cc.to_str()?)
        .ok_or(anyhow!("Could not find max age in cache control header"))?;
    let max_age = cap[1].parse::<u64>()?;
    let keys: KeyMap = response
        .json::<KeyData>()
        .await?
        .keys
        .into_iter()
        .map(|key| Ok((key.kid, DecodingKey::from_rsa_components(&key.n, &key.e)?)))
        .collect::<Result<KeyMap>>()
        .context("Could not get keys from google response")?;
    Ok((keys, Duration::from_secs(max_age)))
}

#[derive(Message, Debug)]
#[rtype(result = "()")]
pub struct RefreshCache {}
impl Handler<RefreshCache> for GoogleKeyStore {
    type Result = ();
    fn handle(&mut self, _msg: RefreshCache, ctx: &mut Self::Context) {
        info!("Refreshing key cache");
        refresh_token(
            self.client
                .get("https://www.googleapis.com/oauth2/v3/certs")
                .insert_header(("User-Agent", "stormgrok"))
                .send(),
        )
        .into_actor(self)
        .then(|res, act, ctx| {
            match res {
                Ok((keys, max_age)) => {
                    act.keys = keys;
                    ctx.notify_later(RefreshCache {}, max_age);
                }
                Err(err) => error!(
                    "encountered error while refreshing google decoding keys: {:?}",
                    err
                ),
            }
            fut::ready(())
        })
        .wait(ctx);
    }
}

#[derive(Message)]
#[rtype(result = "Option<DecodingKey>")]
pub struct ResolveKey {
    pub kid: String,
}
impl Handler<ResolveKey> for GoogleKeyStore {
    type Result = Option<DecodingKey>;
    fn handle(&mut self, msg: ResolveKey, _: &mut Context<Self>) -> Self::Result {
        debug!("Resolving key for 'kid={}'", &msg.kid);
        self.keys.get(&msg.kid).cloned()
    }
}

pub async fn get_key_for_kid(
    key_store_address: Addr<GoogleKeyStore>,
    kid: String,
) -> Result<DecodingKey> {
    match key_store_address
        .send(ResolveKey { kid: kid.clone() })
        .await?
    {
        Some(dec_key) => return Ok(dec_key),
        None => {
            key_store_address.send(RefreshCache {}).await?;
            return key_store_address
                .send(ResolveKey { kid: kid.clone() })
                .await?
                .ok_or(anyhow!(
                    "Google did not supply a DecodingKey for 'kid={kid}'"
                ));
        }
    }
}

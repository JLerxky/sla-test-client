// Copyright Rivtower Technologies LLC.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
// http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use crate::{
    config::Config,
    record::{Record, UnverifiedTX, VerifiedResult},
    time::{get_latest_finalized_minute, ms_to_minute_scale, unix_now},
};
use parking_lot::RwLock;
use serde_json::{json, Value};
use std::sync::{mpsc::Sender, Arc};
use storage_dal::Storage;

pub(crate) struct Client {
    pub config: Arc<RwLock<Config>>,
    pub storage: Storage,
    pub http_client: reqwest::Client,
    pub vr_sender: Sender<VerifiedResult>,
}

impl Client {
    pub async fn sender(&self) {
        let config = self.config.read().clone();
        for data in config.data_for_send {
            let mut record = Record {
                timestamp: unix_now(),
                api: "api/sendTx".to_string(),
                data: data.to_string(),
                resp: json!(null),
                status: 0,
            };
            match self
                .http_client
                .post(format!("{}/{}", config.cache_url, &record.api))
                .header("Content-Type", "application/json")
                .body(record.data.clone())
                .send()
                .await
            {
                Ok(resp) => match resp.json::<Value>().await {
                    Ok(resp) => {
                        info!("Post '{}': {:?}", &record.api, resp);
                        record.add_resp(resp.clone());
                        // save UnverifiedTX
                        if resp["status"].as_u64().unwrap() == 1 {
                            let utx = UnverifiedTX {
                                tx_hash: resp["data"].to_string().replace('\"', ""),
                                sent_timestamp: record.timestamp,
                            };
                            debug!("insert: {:?}", &utx);
                            self.storage.insert(&utx.tx_hash.clone(), utx);
                        }
                    }
                    Err(e) => error!("decoding resp from '{}' failed: {}", &record.api, e),
                },
                Err(e) => error!("Call '{}' failed: {}", &record.api, e),
            }

            // When the call or decode fails, the sent_failed_num at current_minute will increase
            let current_minute = ms_to_minute_scale(record.timestamp);
            let mut vr = self
                .storage
                .get::<VerifiedResult>(&current_minute.to_string())
                .unwrap_or_else(|| {
                    // Record the result of the first two timeout intervals at the current moment
                    let res = self.storage.get::<VerifiedResult>(
                        &get_latest_finalized_minute(
                            record.timestamp,
                            config.validator_timeout,
                            config.chain_block_interval,
                        )
                        .to_string(),
                    );
                    if let Some(res) = res {
                        let _ = self.vr_sender.send(res);
                    }
                    VerifiedResult::new(current_minute)
                });
            if record.status == 1 {
                vr.sent_num += 1;
                info!("sender insert: {:?}", &vr);
            } else {
                vr.sent_failed_num += 1;
                warn!("sender insert: {:?}", &vr);
            }
            self.storage.insert(&current_minute.to_string(), vr);

            debug!("insert: {:?}", &record);
            self.storage.insert(&current_minute.to_string(), record);
        }
    }

    pub async fn validator(&self) {
        let unverified_txs = self.storage.scan::<UnverifiedTX>();
        let config = self.config.read().clone();
        for unverified_tx in unverified_txs {
            let UnverifiedTX {
                tx_hash,
                sent_timestamp,
            } = self
                .storage
                .get_by_path(unverified_tx.unwrap().path())
                .unwrap();
            let current_minute = ms_to_minute_scale(sent_timestamp);
            let mut vr = self
                .storage
                .get::<VerifiedResult>(&current_minute.to_string())
                .unwrap_or_else(|| VerifiedResult::new(current_minute));
            if unix_now() - sent_timestamp
                > (config.validator_timeout as u64 * config.chain_block_interval as u64 * 1000)
            {
                // timeout and failed
                warn!("Failed: {:?}", &tx_hash);
                self.storage.remove::<UnverifiedTX>(&tx_hash);

                vr.failed_num += 1;
                warn!("validator insert: {:?}", &vr);
                self.storage.insert(&current_minute.to_string(), vr);
                continue;
            }

            self.verify_from_cache(tx_hash, vr, current_minute).await;
        }
    }

    async fn verify_from_cache(
        &self,
        tx_hash: String,
        mut vr: VerifiedResult,
        current_minute: u64,
    ) {
        let mut record = Record {
            timestamp: unix_now(),
            api: "api/get-receipt".to_string(),
            data: tx_hash.clone(),
            resp: json!(null),
            status: 0,
        };

        match self
            .http_client
            .get(format!(
                "{}/{}/{}",
                self.config.read().cache_url,
                &record.api,
                &record.data
            ))
            .send()
            .await
        {
            Ok(resp) => match resp.json::<Value>().await {
                Ok(resp) => {
                    info!("Get  '{}/{}': {:?}", &record.api, &record.data, resp);
                    record.add_resp(resp);
                }
                Err(e) => error!("decoding resp from '{}' failed: {}", &record.api, e),
            },
            Err(e) => error!("Call '{}' failed: {}", &record.api, e),
        }

        if record.status == 1 {
            info!("Success: {:?}", &tx_hash);
            self.storage.remove::<UnverifiedTX>(&tx_hash);

            vr.succeed_num += 1;
            info!("validator insert: {:?}", &vr);
            self.storage.insert(&current_minute.to_string(), vr);
        }

        debug!("insert: {:?}", &record);
        self.storage
            .insert(&format!("{}", &record.timestamp), record);
    }
}
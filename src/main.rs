use std::env;
use std::sync::{Arc, Mutex};

use anyhow::Result;

use bitcoin::hashes::{Hash, HashEngine, sha256};
use bitcoin::{Network, Transaction};
use bitcoind_async_client::{Client, traits::*};

use hex;

use image::{RgbaImage, load_from_memory};
use rusqlite::{Connection, params};
use serde::Serialize;
use tokio::task::JoinSet;

struct TxInfo {
    txid: String,
    vin: u32,
    block_height: u64,
    mint_address: String,
    control_block_hex: String,
    feerate_sat_vb: f64,
}

const STARTING_BLOCK: u64 = 908072;
const LAST_PROCESSED_BLOCK_KEY: &str = "last_processed_height";
const NEXT_TX_INDEX_KEY: &str = "next_tx_index";
const LABITBU_INTERNAL_KEY: &str =
    "96053db5b18967b5a410326ecca687441579225a6d190f398e2180deec6e429e";
const BATCH_SIZE: usize = 50;
const TARGET_TX_COUNT: u64 = 10_000;

fn initialize_database_if_needed(conn: &mut Connection) -> Result<()> {
    conn.execute_batch(
        "BEGIN;
        CREATE TABLE IF NOT EXISTS metadata (
            key TEXT PRIMARY KEY,
            value INTEGER NOT NULL
        );
        CREATE TABLE IF NOT EXISTS tx_data (
            id INTEGER PRIMARY KEY,
            txid TEXT NOT NULL,
            vin INTEGER NOT NULL,
            block_height INTEGER NOT NULL,
            mint_address TEXT NOT NULL,
            control_block_hex TEXT NOT NULL,
            fee_rate_sat_vb REAL NOT NULL
        );
        CREATE INDEX IF NOT EXISTS idx_tx_data_block_height ON tx_data (block_height);
        COMMIT;",
    )?;

    let tx = conn.transaction()?;
    let count: u32 = tx.query_row(
        "SELECT count(*) FROM metadata WHERE key = ?",
        params![LAST_PROCESSED_BLOCK_KEY],
        |row| row.get(0),
    )?;
    if count == 0 {
        println!("First run detected. Initializing database state...");
        tx.execute(
            "INSERT INTO metadata (key, value) VALUES (?1, ?2), (?3, ?4)",
            params![
                LAST_PROCESSED_BLOCK_KEY,
                STARTING_BLOCK,
                NEXT_TX_INDEX_KEY,
                1
            ],
        )?;
        println!(
            "Database initialized. Starting block: {}, Next TX Index: 1",
            STARTING_BLOCK
        );
    } else {
        println!("Database already initialized. No action needed.");
    }
    tx.commit()?;
    Ok(())
}

#[tokio::main]
async fn main() -> Result<()> {
    let mut conn = Connection::open("labitbu.sqlite")?;
    initialize_database_if_needed(&mut conn)?;

    if env::args().any(|a| a == "--get-traits") {
        let mut conn = Connection::open("labitbu.sqlite")?;
        get_traits(&mut conn)?;
        return Ok(());
    }

    let db = Arc::new(Mutex::new(conn));

    let rpc_url = env::var("RPC_URL").expect("RPC_URL not set");
    let rpc_username = env::var("RPC_USERNAME").expect("RPC_USERNAME not set");
    let rpc_password = env::var("RPC_PASSWORD").expect("RPC_PASSWORD not set");
    let rpc_client = Client::new(rpc_url, rpc_username, rpc_password, None, None)?;

    find_labitbu(&rpc_client, db.clone()).await?;

    Ok(())
}

async fn find_labitbu(rpc_client: &Client, db: Arc<Mutex<Connection>>) -> Result<()> {
    let key_to_find_bytes =
        hex::decode(LABITBU_INTERNAL_KEY).expect("FATAL: LABITBU_INTERNAL_KEY is not valid hex");

    loop {
        let (start_of_batch_height, mut next_tx_index) = {
            let conn = db.lock().unwrap();
            let height: u64 = conn.query_row(
                "SELECT value FROM metadata WHERE key = ?",
                params![LAST_PROCESSED_BLOCK_KEY],
                |row| row.get(0),
            )?;
            let index: u64 = conn.query_row(
                "SELECT value FROM metadata WHERE key = ?",
                params![NEXT_TX_INDEX_KEY],
                |row| row.get(0),
            )?;
            (height, index)
        };

        let total_indexed = next_tx_index.saturating_sub(1);
        if total_indexed >= TARGET_TX_COUNT {
            println!("Reached target of {} rows. Stopping.", TARGET_TX_COUNT);
            return Ok(());
        }

        let end_of_batch_height = start_of_batch_height + BATCH_SIZE as u64 - 1;
        println!(
            "\nScanning batch of {} blocks ({} -> {})...",
            end_of_batch_height - start_of_batch_height + 1,
            start_of_batch_height,
            end_of_batch_height
        );

        let mut tasks = JoinSet::new();
        for height in start_of_batch_height..=end_of_batch_height {
            let client = rpc_client.clone();
            let key_bytes = key_to_find_bytes.clone();

            tasks.spawn(async move {
                let block_hash = client.get_block_hash(height).await?;
                let block = client.get_block(&block_hash).await?;
                let mut found_txs = Vec::new();

                for tx in &block.txdata {
                    for (vin, input) in tx.input.iter().enumerate() {
                        if input.witness.len() >= 3 {
                            let second_witness_item = &input.witness[2];
                            if second_witness_item.len() == 4129
                                && second_witness_item.windows(4).any(|w| w == b"RIFF")
                                && second_witness_item.windows(4).any(|w| w == b"WEBP")
                                && second_witness_item
                                    .windows(key_bytes.len())
                                    .any(|w| w == key_bytes.as_slice())
                            {
                                let txid = tx.compute_txid();

                                let tx_info =
                                    get_tx_info(&client, tx.clone(), vin as u32, Network::Bitcoin)
                                        .await?;

                                found_txs.push(TxInfo {
                                    txid: txid.to_string(),
                                    vin: vin as u32,
                                    block_height: height,
                                    mint_address: tx_info.address,
                                    control_block_hex: hex::encode(&input.witness[2]),
                                    feerate_sat_vb: tx_info.feerate_sat_vb.unwrap(),
                                });
                                break;
                            }
                        }
                    }
                }
                Ok::<_, anyhow::Error>(found_txs)
            });
        }

        let mut results = Vec::new();
        while let Some(res) = tasks.join_next().await {
            match res {
                Ok(Ok(found_txs)) => results.extend(found_txs),
                Ok(Err(e)) => return Err(e),
                Err(e) => return Err(e.into()),
            }
        }

        if !results.is_empty() {
            results.sort_by_key(|info| (info.block_height, info.vin));
            let mut conn = db.lock().unwrap();
            let tx = conn.transaction()?;
            let total_indexed = next_tx_index.saturating_sub(1);
            let remaining = TARGET_TX_COUNT.saturating_sub(total_indexed);
            if remaining == 0 {
                println!("Reached target of {} rows. Stopping.", TARGET_TX_COUNT);
                return Ok(());
            }

            for info in results.into_iter().take(remaining as usize) {
                println!(
                    "  -> Found TX {} at index {} (Input Addr: {})",
                    info.txid, next_tx_index, &info.mint_address
                );

                tx.execute(
                    "INSERT INTO tx_data (id, txid, vin, block_height, mint_address, control_block_hex, fee_rate_sat_vb) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                    params![
                        next_tx_index,
                        info.txid,
                        info.vin,
                        info.block_height,
                        info.mint_address,
                        info.control_block_hex,
                        info.feerate_sat_vb
                    ],
                )?;
                next_tx_index += 1;
            }
            tx.execute(
                "INSERT OR REPLACE INTO metadata (key, value) VALUES (?1, ?2)",
                params![NEXT_TX_INDEX_KEY, next_tx_index],
            )?;
            tx.commit()?;

            if next_tx_index.saturating_sub(1) >= TARGET_TX_COUNT {
                println!("index {} reached.", TARGET_TX_COUNT);
                return Ok(());
            }
        }

        db.lock().unwrap().execute(
            "INSERT OR REPLACE INTO metadata (key, value) VALUES (?1, ?2)",
            params![LAST_PROCESSED_BLOCK_KEY, end_of_batch_height + 1],
        )?;
        println!(
            "Batch finished. New height: {}. Total indexed: {}.",
            end_of_batch_height + 1,
            next_tx_index - 1
        );
    }
}

pub struct MoreTXInfo {
    pub address: String,
    pub feerate_sat_vb: Option<f64>,
}

pub async fn get_tx_info(
    client: &Client,
    tx: Transaction,
    vin: u32,
    network: Network,
) -> anyhow::Result<MoreTXInfo> {
    let inp = tx.input.get(vin as usize).unwrap();
    let prev_ref = &inp.previous_output;

    let prev_tx_for_vin = client
        .get_raw_transaction_verbosity_one(&prev_ref.txid)
        .await?;
    let spent_out = prev_tx_for_vin
        .transaction
        .output
        .get(prev_ref.vout as usize)
        .unwrap();

    let address = bitcoin::Address::from_script(&spent_out.script_pubkey, network)
        .unwrap()
        .to_string();

    let mut in_sum: u64 = 0;
    for i in &tx.input {
        let prev_verbose = if i.previous_output.txid == prev_ref.txid {
            &prev_tx_for_vin
        } else {
            &match client
                .get_raw_transaction_verbosity_one(&i.previous_output.txid)
                .await
            {
                Ok(v) => v,
                Err(_) => {
                    return Ok(MoreTXInfo {
                        address,
                        feerate_sat_vb: None,
                    });
                }
            }
        };

        let vout_idx = i.previous_output.vout as usize;
        let prev_o = match prev_verbose.transaction.output.get(vout_idx) {
            Some(x) => x,
            None => {
                return Ok(MoreTXInfo {
                    address,
                    feerate_sat_vb: None,
                });
            }
        };

        in_sum = in_sum.saturating_add(prev_o.value.to_sat());
    }

    let out_sum: u64 = tx.output.iter().map(|o| o.value.to_sat()).sum();
    if in_sum < out_sum {
        return Ok(MoreTXInfo {
            address,
            feerate_sat_vb: None,
        });
    }

    let fee = in_sum - out_sum;
    let vsize = tx.vsize() as f64;
    let feerate = (vsize > 0.0).then_some(fee as f64 / vsize);

    Ok(MoreTXInfo {
        address,
        feerate_sat_vb: feerate,
    })
}

const HORNY_HEX: &str = "524946463602000057454250565038580a000000300000002c00003a000049434350c8010000000001c800000000043000006d6e74725247422058595a2007e00001000100000000000061637370000000000000000000000000000000000000000000000000000000010000f6d6000100000000d32d0000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000964657363000000f0000000247258595a00000114000000146758595a00000128000000146258595a0000013c00000014777470740000015000000014725452430000016400000028675452430000016400000028625452430000016400000028637072740000018c0000003c6d6c756300000000000000010000000c656e5553000000080000001c007300520047004258595a200000000000006fa2000038f50000039058595a2000000000000062990000b785000018da58595a2000000000000024a000000f840000b6cf58595a20000000000000f6d6000100000000d32d706172610000000000040000000266660000f2a700000d59000013d000000a5b00000000000000006d6c756300000000000000010000000c656e5553000000200000001c0047006f006f0067006c006500200049006e0063002e00200032003000310036414c504816000000010f30ff111142484098eaff14498ce8ff0430a5892e565038202a0000003003009d012a2d003b003e31188b442221a1115400200304b4800009f8c70f8422c000fefe7c94000000";
const SLEEP_MASK_HEX: &str = "52494646b201000057454250565038580a000000180000002c00003a0000414c50484b000000010f30ff11118251ad6d331ef356031534f8ab88a001d1fe28224c5bc76b9e5947f47f02f85e07a02406327220c54a32ae9524a792243a4905c34abd529d607718ca150629823ae93dfe240056503820f4000000f006009d012a2d003b003e91409947a5a422a130180958b012096200ca5108053d46efbe448da0eaf22c567416d778de411fa9ee31b1b2b1df059d7a7b7de80000fefc5cd0191a3544af14370b23d8949eb5feeff5048b26f38322e13432d937f7ab7baf619b9fad01f185aafac4519fa2307479a03aa9344493f7479d3c5f9656e1070786d8832d22956a04991c9a83108e534d680d74644f44ab9b625a763f544db35f2aefe3efe1bcb146e300fad0c250ef34304a605f0e1ca7880da3bacf53b488abb2109e13fd7a9dbedcd11acbedc24ce40b08bd00538f8c552b28b405769a8773e985bd55076390da3857991aa600000045584946440000004d4d002a00000008000187690004000000010000001a000000000003a00100030000000100010000a0020004000000010000002da0030004000000010000003b00000000";
const PINK_GLASSES_HEX: &str = "524946466e04000057454250565038580a000000300000002c00003a000049434350c8010000000001c800000000043000006d6e74725247422058595a2007e00001000100000000000061637370000000000000000000000000000000000000000000000000000000010000f6d6000100000000d32d0000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000964657363000000f0000000247258595a00000114000000146758595a00000128000000146258595a0000013c00000014777470740000015000000014725452430000016400000028675452430000016400000028625452430000016400000028637072740000018c0000003c6d6c756300000000000000010000000c656e5553000000080000001c007300520047004258595a200000000000006fa2000038f50000039058595a2000000000000062990000b785000018da58595a2000000000000024a000000f840000b6cf58595a20000000000000f6d6000100000000d32d706172610000000000040000000266660000f2a700000d59000013d000000a5b00000000000000006d6c756300000000000000010000000c656e5553000000200000001c0047006f006f0067006c006500200049006e0063002e00200032003000310036414c50484a000000010f30ff1111c26d6cdb4ab5f19012e88294d628cd1aa1027777fd1247f47f02f8d1b6b0236d1cc9040221dc199bc9cfe8a7699a88a769b2707698a6c9b84d8771a7b6260d6cade7af04565038202e020000f00e009d012a2d003b003e31168842a22121180c04ac200304b100698b5262406d80f301fa8dfaddd803d003f667ac03d003f557d277f677e067f70bd803f5daeee6a015b13d46bf55ff81e513e5eff41ee07fa85fe63ac07a007eb3226285bfaa75080afaabfc896ba0d910168a977ba95e7f6166f75e23e3eeed753782400000feff0d6617f271ad56f0d475130c7392e57b0c62bb3c30879b682476eaf9e43cf31ede60e0bd5671f366712265c5ce08dd6b1e642ffe867e452aaa240ef7b2a2c56bcdff7fc6d1b387dc5ab971d5e69a2b93aa8f640c4ef2122cda5e84930bad6d303335f8eef9e6ac449c6fcc565de3133a11ce5536f6ee0637c5c24f31034ef7c837fbf6942a7fe0bef7510a46ab91058a3d9b38d45842a9c58547de31268dc616bcc86992947f6df3752eabbc5eb3073fe5c758f8804e4831651d49b765e1dd1ae31ff478e5528d78d7a6e97f4f105c9dde470c3e846465ae5eec0094eff035f6d727a51b20f1b432742aa6faaa213b75bebdcf62769b4d24017e5dd9d4f88f0827e50b3dbde32dd84adcf1d0304c3fcfee720638527f01d9165c466de91594d3aad09af31438e11d9dd1d6fac37f311d027001a50d6ff00332fcbf4bdc8d17cda4f31d9b2d535a6ce6e3683ac70fff2bff055fa7ca889da3e78b6018cd2550b89c68facadebf3e06b9b480f29b682714e245fdfe82f6e5137abc5cdc586c67847ec009e34ade4653ac5e64b8a53c6629105d018cd876ed5a3a4933f131fc8227c4023da85e684c67800000";
const NORMAL_HEX: &str = "524946461007000057454250565038580a000000200000002c00003a000049434350c8010000000001c800000000043000006d6e74725247422058595a2007e00001000100000000000061637370000000000000000000000000000000000000000000000000000000010000f6d6000100000000d32d0000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000964657363000000f0000000247258595a00000114000000146758595a00000128000000146258595a0000013c00000014777470740000015000000014725452430000016400000028675452430000016400000028625452430000016400000028637072740000018c0000003c6d6c756300000000000000010000000c656e5553000000080000001c007300520047004258595a200000000000006fa2000038f50000039058595a2000000000000062990000b785000018da58595a2000000000000024a000000f840000b6cf58595a20000000000000f6d6000100000000d32d706172610000000000040000000266660000f2a700000d59000013d000000a5b00000000000000006d6c756300000000000000010000000c656e5553000000200000001c0047006f006f0067006c006500200049006e0063002e002000320030003100365650382022050000f01b009d012a2d003b003e3114894322212112bc0600200304b48009c23f827fb3beaff880f0e7ab3fb69cbfb71ddc11e6df8b9fca7fcb7fa5e797b807a99fe79f89dfb77fe678f6b25ff5dfc86e603b897faafe45fbd5fd8bfb178de7ce7fb77d207d087f11fee9fd13f5bffb37c3b7ee3f883fd3ffef7c21f9d7fc3fe407d057f36fe65fd77faf7f63ff4ffda3ff17d38f9aafb04fe96320485393fa7e40bcc395f4ff7f634121a8cd61b6a84b24369b4c38c4535c1a4eceeddf7b212b3cb562043feebb9e6a08990ac828d42d175d8f085cea1bb1f482825e1dbc5ed044933bbfb47adc58cc8000fefffe945c7cddc71859bca695007f47abbce1a7ee93011faf7affd869ceefff40c94dbb4f4329bff7a38dd441f9759fa42850e14c1e5ebafb3333f950bf94f28a6ce0ff2661a219569d042baf2debeca8cf1e1ca9f7fffdc942db0c885db77f0481bc4ffac6bcb9d33e5cdcf3fccd2275db7047ffbf141eb3ec1f1d97f92353d07a1bddc94a385279819f610daf536dde10a32481067c047bf7f47b37683f9e42209da0c98a0699fdfc8a9c2c8dd0698892bc5adaebd2bcf78f7ac8006f19f4a89d526e18c9a45449fa0ea3447ea7158c58e1a17f4d6ef5da90c6b6bddd2b40f0fdfc69cfb228309ab0b27093ec6f90bea7ae407f073177664b6a496014bafc7177da6c9258ed4deed1e4c75041981e450aad46b809348386b7fec02d4ab12d49ba5b10deb4f9981db0a890f7a8660b6f439d5ce1f628262359bcc294106c953a03af0e52314ce2b2c64decc09817f226cf47ffdfffeaa68bbc49d4abf2add10d72a07be10ccaae117777c89ce72da01c797bc2299705ba1b442f82560b4e7d1f7842d2460ccf127c1cb0892dbea1d6c242beb458f1401c12e53ad917b94580bfa5f1de6167596403372fbcb85d0da4699d9bbcdcb69e285ccbf32b24fd8c61cac84db36fffe2b0156afe2490ef465836812ec73ebd049b53629fcc799863ec90b46ff257abf74cd91a9b2becb5adf085f8042b43f03d744cc06488326dfe0767ba4198004227ece0a9970dc393e2b7cb2cdb00007255fb1c83295be09b2f6ba95732b3b365257f9279b375ebae2db26ef99bca247796c9c074b4c74d8758899ce6fee71d08f655835c3d62f9a28bb5f3d4c698c48da7e7f34f27e52af8859783dc211937127f1d53438e9559827fd5a26374a20f8bb912c12ed839ea3489d1636a12268ec4ac85d6a19c8a9c8a8af9c46cc3a36d4ab26c41e933fdf939f0b2bc674fe9f86b45955896164ce939c0df9d18607c6586e365171a5483e853bcdb3dfb703a9ead52f811ba5db8e7b3b158db1615a45b2045c25430c27612ba1d3b2ed0060f78e8b44ea141eadbf44fc8986b825d719d67a61ba81e33a52261e24cb6b1d2540c1eddc232c406ae2ac6af64cc4c8fd18071b043c8f1ce83d55933fb48ef8e9cbb4359d986cdb1330fd9f8fc9a221dbecec349e1478ca17558a2a8e6493cb053918ff6032f8ea02c345fa6e1960a19c46f9eef4ffb29f978d686e9c04137c6a05f6e8a5b8fcc87fff7deb297ff83e151e9b47f492da67de276acfc2eba76c083ef92003550eb3c9abe7f6e21467ed048482b1631f5911a364462f8bbaa46262940f8d02ae78f9a8e9ad93cf3848665f29726934c5edf349298c49bffadd8f005a3e452739ca2350e1f1d320f9a8f1322b582fad7c79907ac183eda60307aafb5158a982bdfffb632d1a522dd5139018c9505374fc670bad6b976be64e731edebfcd2b12af24099f42dff9940ea9c65a59c83144a9d297783cd8fa3ba43a12931ec5a941b390fb52ba97c3a8cf1a0a2074a40000000";
const SAD_HEX: &str = "524946462a07000057454250565038580a000000200000002c00003a000049434350c8010000000001c800000000043000006d6e74725247422058595a2007e00001000100000000000061637370000000000000000000000000000000000000000000000000000000010000f6d6000100000000d32d0000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000964657363000000f0000000247258595a00000114000000146758595a00000128000000146258595a0000013c00000014777470740000015000000014725452430000016400000028675452430000016400000028625452430000016400000028637072740000018c0000003c6d6c756300000000000000010000000c656e5553000000080000001c007300520047004258595a200000000000006fa2000038f50000039058595a2000000000000062990000b785000018da58595a2000000000000024a000000f840000b6cf58595a20000000000000f6d6000100000000d32d706172610000000000040000000266660000f2a700000d59000013d000000a5b00000000000000006d6c756300000000000000010000000c656e5553000000200000001c0047006f006f0067006c006500200049006e0063002e00200032003000310036565038203c050000301b009d012a2d003b003e31148943222121155c0580200304b48009ebcf9b7e117ecefac7e13fb7feaefee0e5f7f997e267f20ff1bfe8b9ebda8ff9dfe267edf7f8ce36cc83fdabf22ff28fd203f103dc0ee2efe99f94feefff5afe75e20df35ff0dea17fca3fb67e4bff4ff852fdb3f12ffb87fe5f831f317f88fc92fa0bfe6bfcabfafff5bfdaefecdff9fe967d807a03fe9232048540b979560f9a46f653c2577ae94dd39015406839a8d9f21bcf0edead1ad6fb13e493246089ab7a275d5d0e16ba05fb76415153331b550b4e8d20d8af7d0fae284139eb50e3adb11248f00000fefffe945c7b87c6d9a63941551be6b2dddf43406e886872bd7fec322d7190e30b76c5fafffc1294676de0e7b21d3956216f40fde75f6439d5face0b71c37442ed21e54fdee2aaac778596c29f62a34b4af3e33fffecfbb86cb556d757e7829f5fffd23445327ec109fde7f95eb555e672431badac6a5c3992c027e9a11b2eaad47a2ee1d09bcb8033a392cef3c5d25ce1790bc1894d243ef9f2423ffb658e77e426301844a0277543d1ce654b69753093e16f3ced6f3e35f4ab4d7c3c6f295f4989a4c69c93d8fd233ddbb69816d019ead8840f761d15dae509a4a4f33d84f82633bf24f61a82e417752814f37cec16e3c8def51e69c29b192620087dec314a944256d3a6fbe30531301d1c97fd8f105db0d2953e6a5d035e0a3490b27de0528b5e4a242f1f5187f7a06fd3ca2228263fa118925cb8fe5a2429ca4d35bd6b81465e18511ea5da0d9fba3a900b66cb5a8f8d9bdc3a5b7f28b83fff7ffa6665147dcdbe34d17900fe835d056005aee53d313d21a8cb1312cc25d78ffeee3a5d4359817742901a7e86461ad7f79cfabd8ba675f637c8982dbc3326711ede6bbc66361e4d32975b2747b6c3fddc2f80547c97d6177892436ed8448ff1035ef6f1327753d7998a3c35762c5c0014b9e00faceff18bd9a3fff0970331c41c13c418e77effe29f46c54b3840ffd6be35dabf6adc6883986e21e1bf8a97bc175290ace2412fd06771a3d1601fddd9b8df24bd7a48fa036210ef4db9e60fff81ea1c8e0c22723bb0f15174f4b45c7134c0fddf2f14c87cbccafba2e3051186e21e7735ae9aeac0252d694cba4e94fa95b56a6d4e196b53dcefa42644613efa527ea1d97960d59c06905476a301b89e1518e5d2569ad0bf6a3d37ffad2eb622ce3bc79f756ee840ffac6cffad253f7817d39de3fffab4494986e6e785cfd89cf8269fd673bad10013a152d184cd2f70f95a852881fbe5eefb4ecfa7ab115d9707a3953aeb5ba1a9c974b8e459113b876747c0bf84c27ea9470e97bf10bbf1c093633ecaf80491b8ad5ddca59b60eae5b6b2a48cc79eba939717d1b2ad71c3541704343de14deb9d31e9f7ecfe9240e139e052def1724be40146caa2da33771cbb83589b7b46bf9fd233770d4f7885dc83360812dfcb9cfa833bcd7ed8ad6025b5a441601309d5539838748d36b3db60b95ef607404d15fe0e22fde1cd57dd12df384ad71c338c65cbb4e50e2dfffdc64d52598724648f9c70da0cb1c1fb84dfc14336cb6eea1d5aeb5ed1bec7f4cb9f9908ffefbe69fbd59ee8823ff7c8fc2fa3b14cfd6db130a7c9ee022a759482ff136484395102cc5bf1faee4e4dc5ff32d09da5aa322b150c91a33fcfa0c12d9ece57189f4c110b0bc272af9ff6c558110b72d6cccb29511ee68228de8be6d49b2947c069d6dba5a47b2114f36329fa0a6e4b1b0d0abcc0c2bdfb8b4321d0c3fffb632b89d49dfa531a051aa780afc66f6ec54c67843f3bd43a14f1095e99545d00b8f35be2140152383064a6930270c2591ac7bf3f3ef23a69b3d414b56583fdf155a88c546c898c9c47b6f10000";
const ANGRY_HEX: &str = "524946465607000057454250565038580a000000200000002c00003a000049434350c8010000000001c800000000043000006d6e74725247422058595a2007e00001000100000000000061637370000000000000000000000000000000000000000000000000000000010000f6d6000100000000d32d0000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000964657363000000f0000000247258595a00000114000000146758595a00000128000000146258595a0000013c00000014777470740000015000000014725452430000016400000028675452430000016400000028625452430000016400000028637072740000018c0000003c6d6c756300000000000000010000000c656e5553000000080000001c007300520047004258595a200000000000006fa2000038f50000039058595a2000000000000062990000b785000018da58595a2000000000000024a000000f840000b6cf58595a20000000000000f6d6000100000000d32d706172610000000000040000000266660000f2a700000d59000013d000000a5b00000000000000006d6c756300000000000000010000000c656e5553000000200000001c0047006f006f0067006c006500200049006e0063002e002000320030003100365650382068050000901c009d012a2d003b003e311888432221a1140d55c4200304b48009c61f877fb01eb3f864eff7ab3fb73fd579e77337f72fc33f0b3f3efc57fe1ffe87fc5f3b3dc03d52ff38fc47fdcbff2dc7b58bffa7fe52ff74fdaae903d903dc7ffabfe587be3fd43fa6f8b0fc9fd057f9bff5dfc89feabf0d5fb17e26ff70ffbbf06fe64ff29f90bfd27ec27f97ff2bfec7fd7ff6b3fa6ffe1fa55f5c9fae9ec29fa70c79214e4fea972b75b2c6d174f6e7ce797fdcbfdad035df3575f97570c8ee8df3cec86df20ecfb2990abc150cafc750b76e3393b6c5ce318be11b62f48b813ed01be4a7ae69d20aa56b51e1a8c000fefffe945c7c2ee7529ed74bb287057c52cfa87d4cca8037e6abfee1e81c3ffa06d386a1fcba55ea0adc46bb0b422730c7fd7f753c30d4c1fc2452cca3e87bd3a5e0db73528ee781110ede637637c6f77c6efb0f0ffffbf198c620459d96f643f0742ffecb9f0e9b8474fb3fad96b183f43b6fc83ff87b343c66ed84f47f8f7829d51dbb7e23be26465e43836d025e2a901ceebe76c0473a5ee799177791f9505b49383e20754dafe96d4f15ff7d6f61e8a9122ae7bed3d562b46911ddb69d275a06b7f191aeb4989d9f87ad1fdb76d1ca66c25fd068634cc29a52929231c1737715cfda4a0e052167481335d81eecd5d119ed6dc00f9483defa505e90d3e4c3eced58c855fd97f727b1190e1ded4d44acdbca707c5122ec1d16d96d9711b9454a0dbbae1552aa5e8ff3c7fc5d8e59972290664f804636fc61c8dd4a3b54ac91806bc655be63226a854d40d1aaa5e01f20e4efe1fff57eb1797c4280204974fba1baa21054d816248365e7fe195db3b757dd620dc8215b23365908787fff2c1de3d5f32915907ce9ffc54364203463e2c7a3077739ccac427e5116ff19d569243702abc0250c21675f93d2135a0afc239a0b9082f2c421f2e60eab9ceb1d2a0e59c77b15a3f4d236d8a394f2fc1c269762bf00a5b3b02d7a243493074954b7a27cdf0eeebb26a2d2cde957ffe3f5487ba6a9a1218fffc210091ea190bab16a4ea397508c84d770cd7d4136fc4d0898f6373820b3a84aae43993cf477dae1de0685a85e5754161664eaa601bec9882980d26056c5f90b6b6c3952855a74dd4e9511b4c2dc797d254437224d9eaeddb4ad75bdc6c452eac2f8da56a95b961233c0c59f01317a9dc9956f50f04f330a908a81f8ef51339bd41ca884e6284fd78727b98755e762fcd17f34c4b65b63d17f63724379113a87ffcd13f08cebd14a285fda254808bd77c7fbad5d21a6db0453b1bf4740ff1bdb88606caf65ff0f2b3d473fb75ce6e1fd8751cfa65b132963287f3ccbf581382f0d457c649ba96038a934ca7f1aed1bea34203a572a3e6be667a0ba7644acca8d9e7bdd4645d497460acad21e72de589c776c3f02db8a9d9bcb327e5652cb05ee96e8b8adc5025137c692b8eb5e96bfc1d81e3ba983a6b0d5a0ad60d09b88126f349563928dac5ca845a2d6b3e9e641693775a0590c9a7903d9985fd3afc189c1c4257add39e34d58df5cc8a2e41f488f8809dee0d9d809a21c5d78dfb3edb75ef95a2b56c03faf8442dc38a19bfff6a20152e1785c2bc67f46f00ced2bffd94b76244982e5138a71ae23a88aa3fe6403d551d2a6567363727152cf72c72d19794fa8e86a86e39b3749292169fe8424fdd9556e62e94fd3ede44698caf7695a4e508ab5d9c6df6f6d763b4dd9117a84924158f942c355ffc4628bf3849139fb0fffc794f4c8e3ae3ff3bbf2ac42f873445d0e838c957016afacff41c2e74098b33873a0899e06c22590135e4f38e9a59ed17fff6c65b94ca178d04ad3ceccebeb8537f19a41f20476573cd0075df403b7b2280caf07a880afb611b8534499d0482850113df153e4a299df0ada3a9b6e64f92be8eb1b17e5e49ba9c1220e03bf3a91c00000";
const SLEEPY_HEX: &str = "52494646f606000057454250565038580a000000200000002c00003a000049434350c8010000000001c800000000043000006d6e74725247422058595a2007e00001000100000000000061637370000000000000000000000000000000000000000000000000000000010000f6d6000100000000d32d0000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000964657363000000f0000000247258595a00000114000000146758595a00000128000000146258595a0000013c00000014777470740000015000000014725452430000016400000028675452430000016400000028625452430000016400000028637072740000018c0000003c6d6c756300000000000000010000000c656e5553000000080000001c007300520047004258595a200000000000006fa2000038f50000039058595a2000000000000062990000b785000018da58595a2000000000000024a000000f840000b6cf58595a20000000000000f6d6000100000000d32d706172610000000000040000000266660000f2a700000d59000013d000000a5b00000000000000006d6c756300000000000000010000000c656e5553000000200000001c0047006f006f0067006c006500200049006e0063002e002000320030003100365650382008050000f01a009d012a2d003b003e31148842a22121180aad54200304b48009c4bf82de6bf905f0e7aabfb23cb5b001e1d5e93f89dfb11fe47d983c31eeabea4bfa6fe1ffee07f92e3e5c85fd7ff25fb90398b7fb2fe3d7bebfda3f9b78a9f847f6ef60bfe41fdebf2bbfb37c2afecff8b1fdbbff67c3ff9a3fc4fe407d05fe98ff66fec7fb41fde3fee7d297af8fd72f609fd1c6672fa0bff4c2ac1298afeaa3f9ff572635d73735d0c38fe41ac66f4b3055504875cdc8f5351112ebcd36ab7a5c022579a7c3a0eb0e39a2d2f042e6f2091e4c63b683eeba2fe35dd0b36be465b3e00000fefffece30cedddb38e13122382d6b19863e25a2f4336e65e5e159ff973cc49f3c78f0cdd3e113fff24d3aacf6ba5b66a2079fde00a1021a7723baec48ccb09564a6ea4e9556924682828b5accdd67da55f6de890f52ffffb3eee20a270e090fdc0fdd7fe91ac67fb05987e115a74ccf1d437edf6992f2de02fe3d7f5c3fa368021e3d903a46f028bb8942eded7464c828f55aab8c5f6674ad4dd9e6df9e4d114a7cbeb44b07773437ea617b9b3eee5c54ab12ecf9790ec6b80a53a77e13a170946fa13df74961da71962490a043369b82fe3b59c5bc3eb15718bfa57b68b88c6a79fb7b6bea233f5e0f51dd71b25d4c8aced2d6431fb34386f0a8c89bc41e7d2d7f1813766591818d9bd2a6bb2fb232a4f7c969a38958ff947d65742800bef05635999eae36165d25881b6cbe325fb23486acfd1b5ba05ec145d72cd8846ca57d9fd84b4491ffbd6fa0be0a7b9ebff57ff25b5469900b7c49bb78d477327b6e6fbe6d346d7882408eb411b48096dbd517a24bcbf3df9a700130e99eb5812f5e3c5374de574c6fa66f419e66e664cfac05f94f1d2fffcbf9f8c169e3ff01d9363f73812f769f71f0da1ea708c3ed06c0efdb98b260e47fff0a210336b831940d58bf309efe2977656321617091ddc817bc0c629b2fb742f8ebdad825ce15e5657405e3bce6526b58b433bd0db6057c264be31e87df7b15ac04b8da04a090fb3a69a7ad48284d87edb6b3289fc5dac94e507b39d3fc2c5957e255a530d497ff4b26883b1804f32347dfbb2cbe2337f043ed0e333645f15598ca448ccc1c611542aa7dfdf0c473dbb9c80a4c3728078d73088755d21e93e6d33ab6f6736dd7e07aa4fe1f823fc41d2ff2940bd34d812b4ddd889d26b4f7078ca2a145230ed97d3b6de6af79afa9cbcddca1839f70c66b56e154dc7afaf943a767bf003856cd72de5db6288d1df195437279cc21bc72b3894a3ccd8c447d566bc86627b62dcc178d02d31f087f66393ba5127635b8c3475b8caa0df99cbd4950fe32ee3a0910fceaa05a34c5eaff596bbfda08700971bbbff3751a8ee0367b1d92f2c1ffe61d539e3c66bea7985e01f6091ff24bf243a946157c9fac0e3cff8d3af901bf3e48b81bf8a8cd45645bb1173bf15c27f5fc8a8460d23ddd72ffd9bd4f837cd04bd25d42a47532fe291f85ba3ff74cfa1c859440f598e60a061d3782ddff652e5ad9219a5dcae66af52106a463d39bc268a3d7b8a739c2042df4a34131e482040140b3566a0555033f50288d8eb15cdc6b36a34a89f3305a15fb007066424747e28a13b1a56942c6ed40f90d2c922ff2ee575eca25115100641307f906f239b6befad78bfffdff7465458fcf11a6f598c79845e641ba350e508f9bc71b6b61936fff797a42fed754fcaf1abdadfb8fb23034c4287135b3b39fbda2c2dc695e125c1dcf458f0d7866cd322218b4481dde014225471765a37f6666b34495253d91a0e73b15b372653f580000";

#[derive(Serialize)]
struct Traits {
    id: u64,
    txid: String,
    body: String,
    accessory: Option<String>,
    parity: u8,
    pool: String,
    image_count: u64,
    fee_rate: f64,
    sat: String,
    mint_address: String,
}

fn parity_from_hex_prefix(s: &str) -> Option<u8> {
    if s.len() < 2 {
        return None;
    }
    match &s[0..2].to_ascii_lowercase()[..] {
        "c0" => Some(0),
        "c1" => Some(1),
        _ => None,
    }
}

fn webp_from_hex(hx: &str) -> anyhow::Result<RgbaImage> {
    Ok(load_from_memory(&hex::decode(hx)?)?.to_rgba8())
}

fn mse_rgba(a: &RgbaImage, b: &RgbaImage) -> f32 {
    if a.dimensions() != b.dimensions() {
        return f32::INFINITY;
    }
    let mut acc = 0f64;
    let mut n = 0usize;
    for (pa, pb) in a.pixels().zip(b.pixels()) {
        for c in 0..4 {
            let d = pa[c] as i32 - pb[c] as i32;
            acc += (d * d) as f64;
            n += 1;
        }
    }
    (acc / n as f64) as f32
}

fn overlay_match_score(target: &RgbaImage, template: &RgbaImage, tol: u8) -> f32 {
    if target.dimensions() != template.dimensions() {
        return 0.0;
    }
    let mut ok = 0usize;
    let mut total = 0usize;
    for (pt, tt) in target.pixels().zip(template.pixels()) {
        if tt[3] > 0 {
            total += 1;
            let mut good = true;
            for c in 0..4 {
                let d = pt[c] as i16 - tt[c] as i16;
                if d.unsigned_abs() > tol.into() {
                    good = false;
                    break;
                }
            }
            if good {
                ok += 1;
            }
        }
    }
    if total == 0 {
        0.0
    } else {
        ok as f32 / total as f32
    }
}

fn decode_first_webp_slice(hex_str: &str) -> anyhow::Result<Vec<u8>> {
    let bytes = hex::decode(hex_str)?;
    let riff = &bytes
        .windows(b"RIFF".len())
        .position(|w| w == b"RIFF")
        .unwrap();

    let end = (riff + 4096).min(bytes.len());
    if end <= *riff {
        anyhow::bail!("Invalid slice range");
    }
    Ok(bytes[*riff..end].to_vec())
}

pub fn get_traits(conn: &mut rusqlite::Connection) -> anyhow::Result<()> {
    let bases = vec![
        ("normal".to_string(), webp_from_hex(NORMAL_HEX)?),
        ("sad".to_string(), webp_from_hex(SAD_HEX)?),
        ("angry".to_string(), webp_from_hex(ANGRY_HEX)?),
        ("sleepy".to_string(), webp_from_hex(SLEEPY_HEX)?),
    ];
    let mut accs = std::collections::HashMap::new();
    accs.insert("horny".to_string(), webp_from_hex(HORNY_HEX)?);
    accs.insert("pinkGlasses".to_string(), webp_from_hex(PINK_GLASSES_HEX)?);
    accs.insert("sleepMask".to_string(), webp_from_hex(SLEEP_MASK_HEX)?);

    let mut stmt = conn.prepare(
        "SELECT id, txid, block_height, control_block_hex, fee_rate_sat_vb, mint_address
     FROM tx_data ORDER BY id ASC",
    )?;
    struct Row {
        id: u64,
        txid: String,
        block_height: u64,
        hex: String,
        fee_rate: f64,
        mint_address: String,
    }
    let rows = stmt
        .query_map([], |r| {
            Ok(Row {
                id: r.get(0)?,
                txid: r.get(1)?,
                block_height: r.get(2)?,
                hex: r.get(3)?,
                fee_rate: r.get(4)?,
                mint_address: r.get(5)?,
            })
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?;

    let tolerance: u8 = 28;
    let min_acc_score: f32 = 0.985;

    std::fs::create_dir_all("labitbu_images")?;

    let mut hits: Vec<Traits> = Vec::new();

    let pools_json = std::fs::read_to_string("mining_pools.json")?;
    let pools_map: std::collections::HashMap<String, String> = serde_json::from_str(&pools_json)?;

    let sats_json = std::fs::read_to_string("first_10k_sats.json")?;
    let sats: Vec<String> = serde_json::from_str(&sats_json)?;

    let mut image_counts: std::collections::HashMap<sha256::Hash, u64> =
        std::collections::HashMap::new();

    for r in &rows {
        let slice = match decode_first_webp_slice(&r.hex) {
            Ok(s) => s,
            Err(_) => continue,
        };
        let cand = match load_from_memory(&slice) {
            Ok(im) => im.to_rgba8(),
            Err(_) => continue,
        };

        let mut engine = sha256::Hash::engine();
        engine.input(&cand.width().to_le_bytes());
        engine.input(&cand.height().to_le_bytes());
        engine.input(cand.as_raw());
        let pix_hash = sha256::Hash::from_engine(engine);

        *image_counts.entry(pix_hash).or_insert(0) += 1;
    }

    for (i, r) in rows.iter().enumerate() {
        let slice = match decode_first_webp_slice(&r.hex) {
            Ok(s) => s,
            Err(_) => continue,
        };
        let cand = match load_from_memory(&slice) {
            Ok(im) => im.to_rgba8(),
            Err(_) => continue,
        };

        let mut engine = sha256::Hash::engine();
        engine.input(&cand.width().to_le_bytes());
        engine.input(&cand.height().to_le_bytes());
        engine.input(cand.as_raw());
        let pix_hash = sha256::Hash::from_engine(engine);

        let count = *image_counts.get(&pix_hash).unwrap_or(&1);

        let mut best_acc: Option<(String, f32)> = None;
        for (name, tpl) in &accs {
            let score = overlay_match_score(&cand, tpl, tolerance);
            if score >= min_acc_score && best_acc.as_ref().map_or(true, |(_, s)| score > *s) {
                best_acc = Some((name.clone(), score));
            }
        }

        let accessory = best_acc.map(|(n, _)| n);

        let mut best = ("unknown".to_string(), f32::INFINITY);
        for (name, tpl) in &bases {
            let e = mse_rgba(&cand, tpl);
            if e < best.1 {
                best = (name.clone(), e);
            }
        }

        let mut base = best.0;

        if accessory.as_deref() == Some("pinkGlasses") && base != "angry" && base != "sad" {
            base = "normal".to_string();
        }

        let acc_label = accessory.as_deref().unwrap_or("none");
        let filename = format!("{}-{}-{}.webp", r.id, base, acc_label);
        let out_path = std::path::Path::new("labitbu_images").join(filename);
        std::fs::write(out_path, &slice)?;

        let parity = parity_from_hex_prefix(&r.hex).unwrap_or_else(|| {
            eprintln!(
                "warn: id {} tx {}: hex didn't start with c0/c1",
                r.id, r.txid
            );
            0
        });

        let pool = pools_map
            .get(&r.block_height.to_string())
            .cloned()
            .unwrap_or_else(|| "Unknown".to_string());

        let sat = sats.get((r.id - 1) as usize).cloned().unwrap();

        hits.push(Traits {
            id: r.id,
            txid: r.txid.clone(),
            body: base,
            accessory,
            parity,
            pool,
            image_count: count,
            fee_rate: (r.fee_rate * 100.0).round() / 100.0,
            sat,
            mint_address: r.mint_address.clone(),
        });

        if i % 1000 == 0 {
            eprintln!("traits: processed {}/{}", i, rows.len());
        }
    }

    std::fs::write("./traits.json", serde_json::to_string_pretty(&hits)?)?;
    println!(
        "Wrote {} entries to traits.json and {} images to ./labitbu_images",
        hits.len(),
        hits.len()
    );
    Ok(())
}

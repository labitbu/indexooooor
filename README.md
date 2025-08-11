# labitbu indexooooooor

Here are a few labitbu stats. There are some more interesting things in regards to "uniqueness" but you guys can figure that out if you care. I just listed some traits I found interesting. I will add more for colors soon.

The full DB and images are in the `labitbu_data.zip` folder in this repo.

## General Info
| Metric | Value |
| --- | --- |
| Unique Mint Addresses | 7,899 |
| Start Block Height | 908,072 |
| Finish Block Height | 908,196 |

## Parity Bits
| Parity Bit | Count |
| --- | --- |
| 0 | 5,070 |
| 1 | 4,930 |

## Art Uniqueness
| Type | Count |
| --- | --- |
| Unique images | 3,028 |
| Most Copies of a Single Image | 37 |

## Body Types
| Body Type | Count |
| --- | --- |
| Normal | 1,675 |
| Sad Boiz | 1,133 |
| Angry | 1,261 |
| Sleepy | 5,928 |
| Unknown | 3 |

## Accessories
| Accessory | Count |
| --- | --- |
| Sleep Masks (lasnoozesnoozes) | 3,450 |
| Pink Glasses | 1,603 |
| Horny | 1,611 |

## Mining Pools
| Mining Pool | Count |
| --- | --- |
| Foundry USA | 37 |
| AntPool | 21 |
| F2Pool | 15 |
| SpiderPool | 11 |
| ViaBTC | 9 |
| Luxor | 5 |
| MARA Pool | 5 |
| SECPOOL | 4 |
| Binance Pool | 3 |
| WhitePool | 3 |
| ULTIMUSPOOL | 3 |
| Unknown | 3 |
| BTC.com | 2 |
| Innoplois Tech | 1 |
| Mining Squared | 1 |
| Ocean (Peak Mining) | 1 |
| Braiins Pool | 1 |

## Fee Rates
| Fee Rate (sat/vB) | Count |
| --- | --- |
| 1.04 | 855 |
| 2.08 | 1,779 |
| 3.12 | 2,396 |
| 4.12 | 1 |
| 4.16 | 1,593 |
| 5.12 | 1 |
| 5.20 | 1,055 |
| 6.24 | 961 |
| 7.29 | 916 |
| 8.33 | 443 |

### Index all txids to db

```bash
cargo run
```

### To generate traits 

```bash
cargo run -- --get-traits
```

### To get sats yourself

```bash
sqlite3 labitbu.sqlite -json "SELECT txid FROM tx_data ORDER BY id LIMIT 10000;" > labitbu.json
```

### Run get_sats.sh script against a pathology node 

- https://github.com/labitbu/pathologies

```bash 
#!/bin/bash

INPUT_FILE="labitbu.json"
OUTPUT_FILE="labitbu_index.json"
PATHOLOGY_SERVER_URL="http://0.0.0.0"

if [ ! -f "$INPUT_FILE" ]; then
  echo "Error: Input file '$INPUT_FILE' not found."
  exit 1
fi

echo "Processing TXIDs from $INPUT_FILE..."

process_txid() {
  local index=$1
  local txid=$2
  local inscription_id="${txid}i0"
  local url="${PATHOLOGY_SERVER_URL}/inscription/${inscription_id}"

  local html_response
  html_response=$(curl -s -f "$url")

  if [ $? -ne 0 ]; then
    echo "Warning: Failed to fetch data for TXID $txid (URL: $url)" >&2
    return
  fi

  local sat
  sat=$(echo "$html_response" | grep -A 1 '<dt>sat</dt>' | tail -n 1 | sed 's/<[^>]*>//g' | tr -d '[:space:]')

  if [[ -n "$sat" ]]; then
    jq -n --argjson index "$index" --arg txid "$txid" --arg sat "$sat" '{"index": $index, "txid": $txid, "sat": $sat}'
    echo "Found sat for $txid" >&2
  else
    echo "Warning: Could not find sat for TXID $txid" >&2
  fi
}

export -f process_txid
export PATHOLOGY_SERVER_URL

jq -r 'to_entries[] | "\(.key + 1) \(.value.txid)"' "$INPUT_FILE" \
  | xargs -n 2 -P 4 bash -c 'process_txid "$@"' _ \
  | jq -s 'sort_by(.index)' > "$OUTPUT_FILE"

echo "Done. Results saved to $OUTPUT_FILE"
```
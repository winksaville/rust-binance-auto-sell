use log::trace;

use rust_decimal::prelude::*;
use rust_decimal_macros::dec;
use std::{
    fs::{File, OpenOptions},
    io::Write,
    path::Path,
};

use crate::{
    binance_avg_price::get_avg_price,
    binance_context::BinanceContext,
    binance_exchange_info::ExchangeInfo,
    binance_order_response::{
        AckTradeResponseRec, FullTradeResponseRec, ResultTradeResponseRec, TestTradeResponseRec,
        TradeResponse, UnknownTradeResponseRec,
    },
    binance_signature::{append_signature, binance_signature, query_vec_u8},
    common::{post_req_get_response, utc_now_to_time_ms, BinanceError, ResponseErrorRec, Side},
};

pub enum MarketQuantityType {
    Quantity(Decimal),
    //QuoteOrderQty(Decimal),
}

pub enum TradeOrderType {
    Market(MarketQuantityType),
    // Limit,
    // StopLoss,
    // StopLossLimit,
    // TakeProfit,
    // TakeProfitLimit,
    // LimitMaker,
}

fn order_log_file(order_log_path: &Path) -> Result<File, Box<dyn std::error::Error>> {
    if let Some(prefix) = order_log_path.parent() {
        if let Err(e) = std::fs::create_dir_all(prefix) {
            panic!("Error creating {:?} e={}", order_log_path, e);
        }
    }

    let order_log_file: File = match OpenOptions::new()
        .create(true)
        .write(true)
        .append(true)
        .open(order_log_path)
    {
        Ok(file) => file,
        Err(e) => {
            return Err(e.into());
        }
    };

    Ok(order_log_file)
}

fn log_order_response<W: Write>(
    mut writer: &mut W,
    order_response: &TradeResponse,
) -> Result<(), Box<dyn std::error::Error>> {
    serde_json::to_writer(&mut writer, order_response)?;
    writer.write_all(b"\n")?;
    Ok(())
}

#[allow(unused)]
async fn convert(
    ctx: &BinanceContext,
    asset: &str,
    value: Decimal,
    other_asset: &str,
) -> Result<Decimal, Box<dyn std::error::Error>> {
    let other_value: Decimal = if asset == other_asset {
        let new_value = value;
        println!(
            "convert: asset: {} value: {} to {}: {}",
            asset, value, other_asset, new_value
        );
        new_value
    } else {
        // Try to directly convert it
        let cvrt_asset_name = asset.to_string() + other_asset;
        match get_avg_price(ctx, &cvrt_asset_name).await {
            Ok(ap) => {
                let new_value = ap.price * value;
                println!(
                    "convert: asset: {} value: {} to {}: {}",
                    asset, value, other_asset, new_value
                );
                new_value
            }
            Err(_) => {
                return Err(format!(
                    "convert error, asset: {} not convertalbe to {}",
                    asset, other_asset
                )
                .into());
            }
        }
    };

    Ok(other_value)
}

async fn convert_commission(
    ctx: &BinanceContext,
    order_response: &FullTradeResponseRec,
    fee_asset: &str,
) -> Result<Decimal, Box<dyn std::error::Error>> {
    let mut commission_value = dec!(0);
    for f in &order_response.fills {
        commission_value += convert(&ctx, &f.commission_asset, f.commission, fee_asset).await?;
    }
    Ok(commission_value)
}

pub async fn binance_new_order_or_test(
    ctx: &BinanceContext,
    ei: &ExchangeInfo,
    symbol: &str,
    side: Side,
    order_type: TradeOrderType,
    test: bool,
) -> Result<TradeResponse, Box<dyn std::error::Error>> {
    let mut writer = order_log_file(&ctx.opts.order_log_path)?;

    let ei_symbol = match ei.get_symbol(symbol) {
        Some(s) => s,
        None => {
            return Err(format!("{} was not found in exchange_info", symbol).into());
        }
    };

    let secret_key = ctx.keys.secret_key.as_bytes();
    let api_key = &ctx.keys.api_key;

    let side_str: &str = side.into();
    let mut params = vec![
        ("recvWindow", "5000"),
        ("symbol", symbol),
        ("side", side_str),
        ("newOrderRespType", "FULL"), // Manually tested, "FULL", "RESULT", "ACK" and "XYZ".
                                      // making ADAUSD buys. "XYZ" generated an error which
                                      // was handled properly.
    ];

    let astring: String;
    match order_type {
        TradeOrderType::Market(MarketQuantityType::Quantity(qty)) => {
            params.push(("type", "MARKET"));
            astring = format!("{}", qty);
            params.push(("quantity", &astring));
        } //_ => return Err("Unknown order_type")?,
    };

    let ts_string: String = format!("{}", utc_now_to_time_ms());
    params.push(("timestamp", ts_string.as_str()));

    trace!("binanace_new_order_or_test: params={:#?}", params);

    let mut query = query_vec_u8(&params);

    // Calculate the signature using sig_key and the data is qs and query as body
    let signature = binance_signature(&secret_key, &[], &query);

    // Append the signature to query
    append_signature(&mut query, signature);

    // Convert to a string
    let query_string = String::from_utf8(query)?;
    trace!("query_string={}", &query_string);

    let path = if test {
        "/api/v3/order/test"
    } else {
        "/api/v3/order"
    };
    let url = "https://api.binance.us".to_string() + path;

    let response = post_req_get_response(api_key, &url, &query_string).await?;
    trace!("response={:#?}", response);
    let response_status = response.status();
    trace!("response_status={:#?}", response_status);
    let response_body = response.text().await?;
    trace!("response_body={:#?}", response_body);

    // Log the response
    let result = if response_status == 200 {
        let order_resp = match serde_json::from_str::<FullTradeResponseRec>(&response_body) {
            Ok(mut full) => {
                full.test = test;
                full.query = query_string.clone();
                full.value_usd = if full.cummulative_quote_qty > dec!(0) {
                    // TODO: Erroring is wrong, maybe dec!(0) plus an error alert sent to the programmer!
                    convert(
                        ctx,
                        &ei_symbol.quote_asset,
                        full.cummulative_quote_qty,
                        "USD",
                    )
                    .await?
                } else {
                    dec!(0)
                };
                full.commission_usd = if !full.fills.is_empty() {
                    // TODO: Erroring is wrong, maybe dec!(0) and an error alert sent to the programmer!
                    convert_commission(&ctx, &full, "USD").await?
                } else {
                    dec!(0)
                };

                TradeResponse::SuccessFull(full)
            }
            Err(_) => match serde_json::from_str::<ResultTradeResponseRec>(&response_body) {
                Ok(mut result) => {
                    result.test = test;
                    result.query = query_string.clone();
                    result.value_usd = if result.status.eq("FILLED") {
                        // TODO: Erroring is wrong, maybe dec!(0) plus an error alert sent to the programmer!
                        convert(
                            ctx,
                            &ei_symbol.quote_asset,
                            result.cummulative_quote_qty,
                            "USD",
                        )
                        .await?
                    } else {
                        dec!(0)
                    };
                    result.commission_usd = dec!(0);

                    TradeResponse::SuccessResult(result)
                }
                Err(_) => match serde_json::from_str::<AckTradeResponseRec>(&response_body) {
                    Ok(mut ack) => {
                        ack.test = test;
                        ack.query = query_string.clone();
                        TradeResponse::SuccessAck(ack)
                    }
                    Err(_) => {
                        if test {
                            TradeResponse::SuccessTest(TestTradeResponseRec {
                                test,
                                query: query_string,
                                response_body,
                            })
                        } else {
                            TradeResponse::SuccessUnknown(UnknownTradeResponseRec {
                                test,
                                query: query_string,
                                response_body,
                                error_internal: "Unexpected trade response body".to_string(),
                            })
                        }
                    }
                },
            },
        };

        trace!(
            "binance_market_order_or_test: symbol={} side={} test={} order_response={:#?}",
            symbol,
            side_str,
            test,
            order_resp
        );
        // TODO: Erroring is wrong, maybe dec!(0) plus an error alert sent to the programmer!
        log_order_response(&mut writer, &order_resp)?;

        Ok(order_resp)
    } else {
        let response_error_rec = ResponseErrorRec::new(
            test,
            response_status.as_u16(),
            &query_string,
            &response_body,
        );
        let binance_error_response = BinanceError::Response(response_error_rec);
        let order_resp = TradeResponse::Failure(binance_error_response.clone());

        // TODO: Erroring is wrong, maybe dec!(0) plus an error alert sent to the programmer!
        log_order_response(&mut writer, &order_resp)?;

        trace!(
            "{}",
            format!(
                "binance_market_order_or_test: symbol={} side={} test={} order_resp={:#?}",
                symbol, side_str, test, order_resp
            )
        );

        Err(binance_error_response.into())
    };

    result
}

#[cfg(test)]
mod test {
    use std::io::{Read, Seek, SeekFrom};

    use super::*;

    const SUCCESS_FULL: &str = r#"{
        "symbol":"ADAUSD",
        "clientOrderId":"2K956RjiRG7mJfk06skarQ",
        "orderId":108342146,
        "orderListId":-1,
        "transactTime":1620435240708,
        "price":"0.0000",
        "origQty":"6.20000000",
        "executedQty":"6.20000000",
        "cummulativeQuoteQty":"10.1463",
        "status":"FILLED",
        "timeInForce":"GTC",
        "type":"MARKET",
        "side":"SELL",
        "fills":[
            {
                "commissionAsset":"BNB",
                "commission":"0.00001209",
                "price":"1.6365",
                "qty":"6.20000000",
                "tradeId":5579228
            }
        ]
    }"#;

    #[tokio::test]
    async fn test_convert() {
        let ctx = BinanceContext::new();

        // Expect to always return the value parameter
        let value_usd = convert(&ctx, "USD", dec!(1234.5678), "USD").await.unwrap();
        assert_eq!(value_usd, dec!(1234.5678));

        // TODO: Need to "mock" get_avg_price so "BNB" asset always returns a specific value.
        let value_usd = convert(&ctx, "BNB", dec!(1), "USD").await.unwrap();
        // assert_eq!(value_usd, dec!(xxx))
        assert!(value_usd > dec!(0));
    }

    #[tokio::test]
    async fn test_convert_commission() {
        let ctx = BinanceContext::new();
        let order_response: FullTradeResponseRec = serde_json::from_str(SUCCESS_FULL).unwrap();

        // TODO: Need to "mock" get_avg_price so order_response.fills[0].commission_asset ("BNB") always returns a specific value.
        let commission_usd = convert_commission(&ctx, &order_response, "USD")
            .await
            .unwrap();
        // assert_eq!(commission_usd, dec!(xxx))
        assert!(commission_usd > dec!(0));
    }

    #[tokio::test]
    async fn test_log_order_response() {
        let order_response: FullTradeResponseRec = serde_json::from_str(SUCCESS_FULL).unwrap();
        let order_resp = TradeResponse::SuccessFull(order_response);

        // Create a cursor buffer and log to it
        let mut buff = std::io::Cursor::new(vec![0; 100]);
        log_order_response(&mut buff, &order_resp).unwrap();
        let buff_len = buff.stream_position().unwrap();

        // Convert to a string so we can inspect it easily, but we must seek to 0 first
        let mut buff_string = String::with_capacity(100);
        buff.seek(SeekFrom::Start(0)).unwrap();
        let buff_string_len = buff
            .read_to_string(&mut buff_string)
            .unwrap()
            .to_u64()
            .unwrap();
        //println!("buff: len: {} string: {}", buff_string_len, buff_string);

        // The length of the string and buffer should be the same
        assert_eq!(buff_len, buff_string_len);

        // Check that it contains 1.6365.  This will assert if the rust_decimal
        // feature, "serde-float", is enabled in Cargo.toml:
        //   rust_decimal = { version = "1.12.4", features = ["serde-arbitrary-precision", "serde-float"] }
        // As we see the following in buff_string:
        //   "price":1.6364999999999998
        //
        // If "serde-float" is NOT enabled:
        //   rust_decimal = { version = "1.12.4", features = ["serde-arbitrary-precision"] }
        // then we see value "correct" price:
        //   "price":"1.6365"
        assert!(buff_string.contains("1.6365"));
    }
}

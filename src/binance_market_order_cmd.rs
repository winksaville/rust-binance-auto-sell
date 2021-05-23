use log::trace;
use rust_decimal::prelude::*;
use rust_decimal_macros::dec;
use structopt::StructOpt;

use crate::{
    binance_account_info::get_account_info,
    binance_avg_price::{get_avg_price, AvgPrice},
    binance_context::BinanceContext,
    binance_exchange_info::{get_exchange_info, ExchangeInfo},
    binance_order_response::TradeResponse,
    binance_orders::get_open_orders,
    binance_trade::{
        self, binance_new_order_or_test, order_log_file, MarketQuantityType, TradeOrderType,
    },
    binance_verify_order::{
        adj_quantity_verify_lot_size, verify_max_position, verify_min_notional, verify_open_orders,
        verify_quanity_is_greater_than_free,
    },
    common::{InternalErrorRec, Side},
    ier_new,
};
use binance_trade::log_order_response;

extern crate function_name;
use function_name::named;

#[named]
pub async fn market_order(
    ctx: &BinanceContext,
    ei: &ExchangeInfo,
    symbol_name: &str,
    quantity: Decimal,
    side: Side,
    test: bool,
) -> Result<TradeResponse, Box<dyn std::error::Error>> {
    let mut log_writer = order_log_file(&ctx.opts.order_log_path)?;

    let mut quantity = quantity;
    if quantity <= dec!(0) {
        let tr = TradeResponse::FailureInternal(ier_new!(
            1,
            &format!("adjusted quantity: {} <= 0", quantity)
        ));
        log_order_response(&mut log_writer, &tr)?;
        return Err(tr.into());
    }
    trace!("symbol_name: {} quantity: {}", symbol_name, quantity);

    let symbol = match ei.get_symbol(&symbol_name) {
        Some(s) => s,
        None => {
            let tr = TradeResponse::FailureInternal(ier_new!(
                2,
                &format!("No asset named {}", symbol_name)
            ));
            log_order_response(&mut log_writer, &tr)?;
            return Err(tr.into());
        }
    };
    trace!("Got symbol");

    let ai = get_account_info(ctx).await?;
    trace!("Got AccountInfo");

    let open_orders = get_open_orders(ctx, &symbol.symbol).await?;

    // Verify the maximum number of orders isn't exceeded.
    verify_open_orders(&open_orders, symbol)?;

    // Adjust quantity and verify the quantity meets the LotSize criteria
    quantity = adj_quantity_verify_lot_size(symbol, quantity);

    // Could have gone zero, if so return an error
    if quantity <= dec!(0) {
        let tr = TradeResponse::FailureInternal(ier_new!(
            3,
            &format!("adjusted quantity: {} <= 0", quantity)
        ));
        log_order_response(&mut log_writer, &tr)?;
        return Err(tr.into());
    }

    // Verify the quantity meets the min_notional criteria
    let avg_price: AvgPrice = get_avg_price(ctx, &symbol.symbol).await?;
    if let Err(e) = verify_min_notional(&avg_price, symbol, quantity) {
        let tr = TradeResponse::FailureInternal(ier_new!(4, &e.to_string()));
        log_order_response(&mut log_writer, &tr)?;
        return Err(tr.into());
    }

    // Verify MaxPosition
    verify_max_position(&ai, &open_orders, symbol, quantity)?;

    // Could use if matches!(side, Side::SELL) but this is safer if Side changes
    match side {
        Side::SELL => {
            verify_quanity_is_greater_than_free(&ai, symbol, quantity)?;
        }
        Side::BUY => {}
    }

    let tr = binance_new_order_or_test(
        ctx,
        &mut log_writer,
        ei,
        &symbol_name,
        side,
        TradeOrderType::Market(MarketQuantityType::Quantity(quantity)),
        test,
    )
    .await?;
    trace!("market trade reponse: {:#?}", tr);

    Ok(tr)
}

#[derive(Debug, Clone, Default, StructOpt)]
pub struct MarketCmdRec {
    /// Symbol name
    pub sym_name: String,

    /// Number of shares
    pub quantity: Decimal,

    /// Enable test mode
    #[structopt(short = "t", long)]
    test: bool,
}

pub async fn buy_market_order_cmd(
    ctx: &BinanceContext,
    rec: &MarketCmdRec,
) -> Result<(), Box<dyn std::error::Error>> {
    trace!("buy_market_order: rec: {:#?}", rec);

    let ei = &get_exchange_info(ctx).await?;
    let tr = market_order(ctx, ei, &rec.sym_name, rec.quantity, Side::BUY, rec.test).await?;
    println!("{}", tr);

    Ok(())
}

pub async fn sell_market_order_cmd(
    ctx: &BinanceContext,
    rec: &MarketCmdRec,
) -> Result<(), Box<dyn std::error::Error>> {
    trace!("sell_market_order: rec: {:#?}", rec);

    let ei = &get_exchange_info(ctx).await?;
    let tr = market_order(ctx, ei, &rec.sym_name, rec.quantity, Side::SELL, rec.test).await?;
    println!("{}", tr);

    Ok(())
}

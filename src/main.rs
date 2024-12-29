
/// Stable Channels in LDK 
/// Contents
/// Main data structure and helper types are in `types.rs`.
/// The price feed config and logic is in price_feeds.rs.
/// This present file incldues LDK set-up, program initialization,
/// a command-line interface, and the core stability logic.
/// We have three different services: exchange, user, and lsp

mod types;
mod price_feeds;

/// This is used for advanced LSP features only
/// pulled from https://github.com/tnull/ldk-node-hack
// extern crate ldk_node_hack;

use std::{
    io::{self, Write},
    str::FromStr,
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use ldk_node::{
    bitcoin::{self, secp256k1::PublicKey, Network},
    lightning::{
        ln::{msgs::SocketAddress, ChannelId},
        offers::offer::Offer,
    },
    lightning_invoice::{payment, Bolt11Invoice},
    Builder, ChannelConfig, Node,
};

use ldk_node::bitcoin::Address;
use price_feeds::{calculate_median_price, fetch_prices, set_price_feeds};
use reqwest::blocking::Client;
use types::{Bitcoin, StableChannel, USD};

extern crate ldk_node_hack;

/// LDK set-up and initialization
fn make_node(alias: &str, port: u16, lsp_pubkey:Option<PublicKey>) -> ldk_node::Node {
    let mut builder = Builder::new();

    // If we pass in an LSP pubkey then set your liquidity source
    if let Some(lsp_pubkey) = lsp_pubkey {
        println!("{}", lsp_pubkey.to_string());
        let address = "127.0.0.1:9377".parse().unwrap();
        builder.set_liquidity_source_lsps2(address, lsp_pubkey, Some("00000000000000000000000000000000".to_owned()));
    }

    builder.set_network(Network::Signet);
   
    // If this doesn't work, try the other one
    builder.set_esplora_server("https://mutinynet.com/api/".to_string());
     // builder.set_esplora_server("https://mutinynet.ltbl.io/api".to_string());

    // Don't need gossip right now. Also interferes with Bolt12 implementation.
    // builder.set_gossip_source_rgs("https://mutinynet.ltbl.io/snapshot".to_string());
    builder.set_storage_dir_path(("./data/".to_owned() + alias).to_string());
    let _ = builder.set_listening_addresses(vec![format!("127.0.0.1:{}", port).parse().unwrap()]);

    let node = builder.build().unwrap();
    node.start().unwrap();
    let public_key: PublicKey = node.node_id();

    let listening_addresses: Vec<SocketAddress> = node.listening_addresses().unwrap();

    if let Some(first_address) = listening_addresses.first() {
        println!("");
        println!("Actor Role: {}", alias);
        println!("Public Key: {}", public_key);
        println!("Internet Address: {}", first_address);
        println!("");
    } else {
        println!("No listening addresses found.");
    }

    return node;
}

fn make_hack_node(alias: &str, port: u16) -> ldk_node_hack::Node {

    let mut builder = ldk_node_hack::Builder::new();

    let promise_secret = [0u8; 32];
    builder.set_liquidity_provider_lsps2(promise_secret);

    builder.set_network(Network::Signet);

    // If this doesn't work, try the other one
   
    // builder.set_esplora_server("https://mempool.emzy.de/api".to_string());
    
    builder.set_esplora_server("https://mutinynet.com/api/".to_string());
    // builder.set_esplora_server("https://mutinynet.ltbl.io/api".to_string());

    // Don't need gossip right now. Also interferes with Bolt12 implementation.
    // builder.set_gossip_source_rgs("https://mutinynet.ltbl.io/snapshot".to_string());
    builder.set_storage_dir_path(("./data/".to_owned() + alias).to_string());
    let _ = builder.set_listening_addresses(vec![format!("127.0.0.1:{}", port).parse().unwrap()]);

    let node = builder.build().unwrap();

    node.start().unwrap();
    let public_key: PublicKey = node.node_id();

    let listening_addresses: Vec<SocketAddress> = node.listening_addresses().unwrap();

    if let Some(first_address) = listening_addresses.first() {
        println!("");
        println!("Actor Role: {}", alias);
        println!("Public Key: {}", public_key);
        println!("Internet Address: {}", first_address);
        println!("");
    } else {
        println!("No listening addresses found.");
    }
    return node;
}

/// Core stability logic  
fn check_stability(node: &ldk_node_hack::Node, mut sc: StableChannel) -> StableChannel {
    sc.latest_price = fetch_prices(&Client::new(), &set_price_feeds())
        .and_then(|prices| calculate_median_price(prices))
        .unwrap_or(0.0);

        if let Some(channel) = node.list_channels().iter().find(|c| c.channel_id == sc.channel_id) {
        sc = update_balances(sc, Some(channel.clone()));
    }

    let mut dollars_from_par: USD = sc.stable_receiver_usd - sc.expected_usd;
    let mut percent_from_par = ((dollars_from_par / sc.expected_usd) * 100.0).abs();

    println!("{:<25} {:>15}", "Expected USD:", sc.expected_usd);
    println!("{:<25} {:>15}", "User USD:", sc.stable_receiver_usd);
    println!("{:<25} {:>5}", "Percent from par:", format!("{:.2}%\n", percent_from_par));

    println!("{:<25} {:>15}", "User BTC:", sc.stable_receiver_btc);
    println!("{:<25} {:>15}", "LSP USD:", sc.stable_provider_usd);

    enum Action {
        Wait,
        Pay,
        DoNothing,
        HighRisk,
    }

    let action = if percent_from_par < 0.1 {
        Action::DoNothing
    } else {
        let is_receiver_below_expected: bool = sc.stable_receiver_usd < sc.expected_usd;
        
        match (sc.is_stable_receiver, is_receiver_below_expected, sc.risk_level > 100) {
            (_, _, true) => Action::HighRisk, // High risk scenario
            (true, true, false) => Action::Wait,   // We are User and below peg, wait for payment
            (true, false, false) => Action::Pay,   // We are User and above peg, need to pay
            (false, true, false) => Action::Pay,   // We are LSP and below peg, need to pay
            (false, false, false) => Action::Wait, // We are LSP and above peg, wait for payment
        }
    };

    match action {
        Action::DoNothing => println!("\nDifference from par less than 0.1%. Doing nothing."),
        Action::Wait => {
            println!("\nWaiting 10 seconds and checking on payment...\n");
            std::thread::sleep(std::time::Duration::from_secs(10));

            if let Some(channel) = node
                .list_channels()
                .iter()
                .find(|c| c.channel_id == sc.channel_id) {sc = update_balances(sc, Some(channel.clone()));
            }

            println!("{:<25} {:>15}", "Expected USD:", sc.expected_usd);
            println!("{:<25} {:>15}", "User USD:", sc.stable_receiver_usd);

            dollars_from_par = sc.stable_receiver_usd - sc.expected_usd;
            percent_from_par = ((dollars_from_par / sc.expected_usd) * 100.0).abs();

            println!("{:<25} {:>5}", "Percent from par:", format!("{:.2}%\n", percent_from_par));

            println!("{:<25} {:>15}", "LSP USD:", sc.stable_provider_usd);
        },
        Action::Pay => {
            println!("\nPaying the difference...\n");
            
            let amt = USD::to_msats(dollars_from_par, sc.latest_price);

            // let result = node.bolt12_payment().send_using_amount(&sc.counterparty_offer,Some("here ya go".to_string()),amt);

            // This is keysend / spontaenous payment code you can use if Bolt12 doesn't work
            
            // First, ensure we are connected
            // let address = format!("127.0.0.1:9737").parse().unwrap();
            // let result = node.connect(sc.counterparty, address, true);

            // if let Err(e) = result {
            //     println!("Failed to connect with : {}", e);
            // } else {
            //     println!("Successfully connected.");
            // }

            let result = node
                .spontaneous_payment()
                .send(amt, sc.counterparty);
            match result {
                Ok(payment_id) => println!("Payment sent successfully with payment ID: {}", payment_id),
                Err(e) => println!("Failed to send payment: {}", e),
            }

            match result {
                Ok(payment_id) => println!("Payment sent successfully with ID: {:?}", payment_id.to_string()),
                Err(e) => eprintln!("Failed to send payment: {:?}", e),
            }
        },
        Action::HighRisk => {
            println!("Risk level high. Current risk level: {}", sc.risk_level);
        },
    }

    sc
}

fn update_balances(mut sc: StableChannel, channel_details: Option<ldk_node_hack::ChannelDetails>) -> StableChannel {
    let (our_balance, their_balance) = match channel_details {
        Some(channel) => {
            let unspendable_punishment_sats = channel.unspendable_punishment_reserve.unwrap_or(0);
            let our_balance_sats = (channel.outbound_capacity_msat / 1000) + unspendable_punishment_sats;
            let their_balance_sats = channel.channel_value_sats - our_balance_sats;
            (our_balance_sats, their_balance_sats)
        }
        None => (0, 0), // Handle the case where channel_details is None
    };

    // Update balances based on whether this is a User or provider
    if sc.is_stable_receiver {
        sc.stable_receiver_btc = Bitcoin::from_sats(our_balance);
        sc.stable_receiver_usd = USD::from_bitcoin(sc.stable_receiver_btc, sc.latest_price);
        sc.stable_provider_btc = Bitcoin::from_sats(their_balance);
        sc.stable_provider_usd = USD::from_bitcoin(sc.stable_provider_btc, sc.latest_price);
    } else {
        sc.stable_provider_btc = Bitcoin::from_sats(our_balance);
        sc.stable_provider_usd = USD::from_bitcoin(sc.stable_provider_btc, sc.latest_price);
        sc.stable_receiver_btc = Bitcoin::from_sats(their_balance);
        sc.stable_receiver_usd = USD::from_bitcoin(sc.stable_receiver_btc, sc.latest_price);
    }

    sc // Return the modified StableChannel
}

fn get_user_input(prompt: &str) -> (String, Option<String>, Vec<String>) {
    let mut input = String::new();
    print!("{}", prompt);
    io::stdout().flush().unwrap();
    io::stdin().read_line(&mut input).unwrap();

    let input = input.trim().to_string();

    let mut parts = input.split_whitespace();
    let command = parts.next().map(|s| s.to_string());
    let args: Vec<String> = parts.map(|s| s.to_string()).collect();

    (input, command, args)
}

/// Program initialization and command-line-interface
fn main() {
    #[cfg(feature = "exchange")] {
        let exchange = make_node("exchange", 9735, None);
    
        loop {
            let (input, command, args) = get_user_input("Enter command for exchange: ");

            match (command.as_deref(), args.as_slice()) {
                (Some("openchannel"), args) => {
                    if args.len() != 3 {
                        println!("Error: 'openchannel' command requires three parameters: <node_id>, <listening_address>, and <sats>");
                        return;
                    }

                    let node_id_str = &args[0];
                    let listening_address_str = &args[1];
                    let sats_str = &args[2];

                    let lsp_node_id = node_id_str.parse().unwrap();
                    let lsp_net_address: SocketAddress = listening_address_str.parse().unwrap();
                    let sats: u64 = sats_str.parse().unwrap();
            
                    let channel_config: Option<Arc<ChannelConfig>> = None;    
                    let announce_channel = true;

                    match exchange.connect_open_channel(lsp_node_id, lsp_net_address, sats, Some(sats/2), channel_config, announce_channel) {
                        Ok(_) => println!("Channel successfully opened to {}", node_id_str),
                        Err(e) => println!("Failed to open channel: {}", e),
                    }
                },
                (Some("onchainsend"), [address_str]) => {
                    match Address::from_str(address_str) {
                        Ok(addr) => match addr.require_network(exchange.config().network) {
                            Ok(addr_checked) => {
                                match exchange.onchain_payment().send_all_to_address(&addr_checked) {
                                    Ok(txid) => println!("{}", txid),
                                    Err(e) => eprintln!("Error: {}", e),
                                }
                            }
                            Err(_) => eprintln!("Invalid address for network"),
                        }
                        Err(_) => eprintln!("Invalid address"),
                    }                
                },
                (Some("getaddress"), []) => {
                    let funding_address = exchange.onchain_payment().new_address();
                    match funding_address {
                        Ok(fund_addr) => println!("Exchange Funding Address: {}", fund_addr),
                        Err(e) => println!("Error getting funding address: {}", e),
                    }
                },
                (Some("balance"), []) => {
                    let balances = exchange.list_balances();
                    let onchain_balance = Bitcoin::from_sats(balances.total_onchain_balance_sats);
                    let lightning_balance = Bitcoin::from_sats(balances.total_lightning_balance_sats);
                    println!("Exchange On-Chain Balance: {}", onchain_balance);
                    println!("Exchange Lightning Balance: {}", lightning_balance);
                },
                (Some("closeallchannels"), []) => {
                    for channel in exchange.list_channels().iter() {
                        let user_channel_id = channel.user_channel_id;
                        let counterparty_node_id = channel.counterparty_node_id;
                        let _ = exchange.close_channel(&user_channel_id, counterparty_node_id);
                    }
                    print!("Closing all channels.")
                },
                (Some("listallchannels"), []) => {
                    let channels = exchange.list_channels();
                    if channels.is_empty() {
                        println!("No channels found.");
                    } else {
                        println!("User Channels:");
                        for channel in channels.iter() {
                            println!("--------------------------------------------");
                            println!("Channel ID: {}", channel.channel_id);
                            println!("Channel Value: {}", Bitcoin::from_sats(channel.channel_value_sats));
                            // println!("Our Balance: {}", Bitcoin::from_sats(channel.outbound_capacity_msat / 1000));
                            // println!("Their Balance: {}", Bitcoin::from_sats(channel.inbound_capacity_msat / 1000));
                            println!("Channel Ready?: {}", channel.is_channel_ready);
                        }
                        println!("--------------------------------------------");
                    }
                },
                (Some("getinvoice"), [sats]) => {
                    if let Ok(sats_value) = sats.parse::<u64>() {
                        let msats = sats_value * 1000;
                        let bolt11 = exchange.bolt11_payment();
                        let invoice = bolt11.receive(msats, "test invoice", 6000);
                        match invoice {
                            Ok(inv) => println!("Exchange Invoice: {}", inv),
                            Err(e) => println!("Error creating invoice: {}", e)
                        }
                    } else {
                        println!("Invalid sats value provided");
                    }
                },
                (Some("openchannel"), args) => {
                    if args.len() != 3 {
                        println!("Error: 'openchannel' command requires three parameters: <node_id>, <listening_address>, and <sats>");
                        return;
                    }

                    // TODO - set zero reserve
                    // ChannelHandshakeConfig::their_channel_reserve_proportional_millionths
                    // https://docs.rs/lightning/latest/lightning/util/config/struct.ChannelHandshakeConfig.html#structfield.their_channel_reserve_proportional_millionths
                    
                    // https://docs.rs/lightning/latest/lightning/util/config/struct.ChannelHandshakeLimits.html#structfield.max_channel_reserve_satoshis

                    let node_id_str = &args[0];
                    let listening_address_str = &args[1];
                    let sats_str = &args[2];

                    let lsp_node_id = node_id_str.parse().unwrap();
                    let lsp_net_address: SocketAddress = listening_address_str.parse().unwrap();
                    let sats: u64 = sats_str.parse().unwrap();
                    let push_msat = (sats / 2) * 1000;
            
                    let channel_config: Option<Arc<ChannelConfig>> = None;    
                    
                    let announce_channel = true;

                    match exchange.connect_open_channel(lsp_node_id, lsp_net_address, sats, Some(push_msat), channel_config, announce_channel) {
                        Ok(_) => println!("Channel successfully opened to {}", node_id_str),
                        Err(e) => println!("Failed to open channel: {}", e),
                    }
                
                },
                (Some("payjitinvoice"), [invoice_str]) | (Some("payinvoice"), [invoice_str]) => {
                    let bolt11_invoice = invoice_str.parse::<Bolt11Invoice>();
                    match bolt11_invoice {
                        Ok(invoice) => {
                            match exchange.bolt11_payment().send(&invoice) {
                                Ok(payment_id) => println!("Payment sent from Exchange with payment_id: {}", payment_id),
                                Err(e) => println!("Error sending payment from Exchange: {}", e)
                            }
                        },
                        Err(e) => println!("Error parsing invoice: {}", e),
                    }
                },
                
                (Some("exit"), _) => break,
                _ => println!("Unknown command or incorrect arguments: {}", input),
                }
        }
    }

    #[cfg(feature = "user")] {
        let lsp_pubkey_str = "03ebdb4d14e3101c1d63e3d5555db2d15bc50d32bc30919b7dfd3d35609b978ff4";
        let lsp_pubkey = PublicKey::from_str(lsp_pubkey_str).ok();
        let user = make_node("user", 9736, lsp_pubkey);
        let mut their_offer: Option<Offer> = None;

        loop {
            let (_input, command, args) = get_user_input("Enter command for user: ");

            match (command.as_deref(), args.as_slice()) {
                (Some("settheiroffer"), [their_offer_str]) => {
                    their_offer = Some(Offer::from_str(&their_offer_str).unwrap());
                    println!("Offer set.")
                }
                (Some("getouroffer"),[]) => {
                    let our_offer: Offer = user.bolt12_payment().receive_variable_amount("thanks").unwrap();
                    println!("{}", our_offer);
                },
                // Sample start command below:
                // startstablechannel CHANNEL_ID IS_STABLE_RECEIVER EXPECTED_DOLLAR_AMOUNT EXPECTED_BTC_AMOUNT
                // startstablechannel 44c105c0f12c47ef4f573928448fb1c662fd61289b0baf93537f03075aa99010 true 305.0 0
                (Some("startstablechannel"), [channel_id, is_stable_receiver, expected_dollar_amount, native_amount_sats]) => {
                    let channel_id = channel_id.to_string();
                    let is_stable_receiver = is_stable_receiver.parse::<bool>().unwrap_or(false);
                    let expected_dollar_amount = expected_dollar_amount.parse::<f64>().unwrap_or(0.0);
                    let native_amount_sats = native_amount_sats.parse::<f64>().unwrap_or(0.0);

                    let counterparty = user.list_channels()
                        .iter()
                        .find(|channel| {
                            println!("channel_id: {}", channel.channel_id);
                            channel.channel_id.to_string() == channel_id
                        })
                        .map(|channel| channel.counterparty_node_id)
                        .expect("Failed to find channel with the specified sID");
                
                    let channel_id_bytes: [u8; 32] = hex::decode(channel_id)
                        .expect("Invalid hex string")
                        .try_into()
                        .expect("Decoded channel ID has incorrect length");

                    let mut stable_channel = StableChannel {
                        channel_id: ChannelId::from_bytes(channel_id_bytes),
                        is_stable_receiver,  
                        counterparty,
                        expected_usd: USD::from_f64(expected_dollar_amount),
                        expected_btc: Bitcoin::from_btc(native_amount_sats),
                        stable_receiver_btc: Bitcoin::from_btc(0.0),
                        stable_provider_btc: Bitcoin::from_btc(0.0),  
                        stable_receiver_usd: USD::from_f64(0.0),
                        stable_provider_usd: USD::from_f64(0.0),
                        risk_level: 0, 
                        timestamp: 0,
                        formatted_datetime: "2021-06-01 12:00:00".to_string(), 
                        payment_made: false,
                        sc_dir: "/path/to/sc_dir".to_string(),
                        latest_price: 0.0, 
                        prices: "".to_string()
                    };

                    println!("Stable Channel created: {:?}", stable_channel.channel_id.to_string());

                    loop {
                        let now = SystemTime::now();
                        let now_duration = now.duration_since(UNIX_EPOCH).unwrap();
                    
                        let now_secs = now_duration.as_secs();
                    
                        let next_10_sec = ((now_secs / 60) + 1) * 60;
                        let next_10_sec_duration = Duration::from_secs(next_10_sec);
                    
                        let sleep_duration = next_10_sec_duration
                            .checked_sub(now_duration)
                            .unwrap_or_else(|| Duration::from_secs(0));
                    
                        // Sleep until the next 60-second mark
                        std::thread::sleep(sleep_duration);
                    
                        println!();
                        println!(
                            "\nChecking stability for channel {}...\n",
                            stable_channel.channel_id
                        );
                        stable_channel = check_stability(&user, stable_channel);
                    }
                },
                (Some("getaddress"), []) => {
                    let funding_address = user.onchain_payment().new_address();
                    match funding_address {
                        Ok(fund_addr) => println!("User Funding Address: {}", fund_addr),
                        Err(e) => println!("Error getting funding address: {}", e),
                    }
                },
                (Some("openchannel"), args) => {
                    if args.len() != 3 {
                        println!("Error: 'openchannel' command requires three parameters: <node_id>, <listening_address>, and <sats>");
                        return;
                    }

                    // TODO - set zero reserve
                    // ChannelHandshakeConfig::their_channel_reserve_proportional_millionths
                    // https://docs.rs/lightning/latest/lightning/util/config/struct.ChannelHandshakeConfig.html#structfield.their_channel_reserve_proportional_millionths
                    
                    // https://docs.rs/lightning/latest/lightning/util/config/struct.ChannelHandshakeLimits.html#structfield.max_channel_reserve_satoshis

                    let node_id_str = &args[0];
                    let listening_address_str = &args[1];
                    let sats_str = &args[2];

                    let lsp_node_id = node_id_str.parse().unwrap();
                    let lsp_net_address: SocketAddress = listening_address_str.parse().unwrap();
                    let sats: u64 = sats_str.parse().unwrap();
                    let push_msat = (sats / 2) * 1000;
            
                    let channel_config: Option<Arc<ChannelConfig>> = None;    
                    
                    let announce_channel = true;

                    match user.connect_open_channel(lsp_node_id, lsp_net_address, sats, Some(push_msat), channel_config, announce_channel) {
                        Ok(_) => println!("Channel successfully opened to {}", node_id_str),
                        Err(e) => println!("Failed to open channel: {}", e),
                    }
                
                },
                (Some("balance"), []) => {
                    let balances = user.list_balances();
                    let onchain_balance = Bitcoin::from_sats(balances.total_onchain_balance_sats);
                    let lightning_balance = Bitcoin::from_sats(balances.total_lightning_balance_sats);
                    println!("User On-Chain Balance: {}", onchain_balance);
                    println!("Stable Receiver Lightning Balance: {}", lightning_balance);
                },
                (Some("connecttolsp"), []) => {
                    let lsp_pubkey = PublicKey::from_str(lsp_pubkey_str).unwrap();
                    let address: SocketAddress = SocketAddress::from_str("127.0.0.1:9737").unwrap();
                    user.connect(lsp_pubkey,address , true).unwrap();
                },
                (Some("closeallchannels"), []) => {
                    for channel in user.list_channels().iter() {
                        let user_channel_id = channel.user_channel_id;
                        let counterparty_node_id = channel.counterparty_node_id;
                        let _ = user.close_channel(&user_channel_id, counterparty_node_id);
                    }
                    print!("Closing all channels.")
                },
                (Some("listallchannels"), []) => {
                    let channels = user.list_channels();
                    if channels.is_empty() {
                        println!("No channels found.");
                    } else {
                        println!("User Channels:");
                        for channel in channels.iter() {
                            println!("--------------------------------------------");
                            println!("Channel ID: {}", channel.channel_id);
                            println!("Channel Value: {}", Bitcoin::from_sats(channel.channel_value_sats));
                            // println!("Our Balance: {}", Bitcoin::from_sats(channel.outbound_capacity_msat / 1000));
                            // println!("Their Balance: {}", Bitcoin::from_sats(channel.inbound_capacity_msat / 1000));
                            println!("Channel Ready?: {}", channel.is_channel_ready);
                        }
                        println!("--------------------------------------------");
                    }
                },
                (Some("getinvoice"), [sats]) => {
                    if let Ok(sats_value) = sats.parse::<u64>() {
                        let msats = sats_value * 1000;
                        let bolt11: ldk_node::payment::Bolt11Payment = user.bolt11_payment();
                        let invoice = bolt11.receive(msats, "test invoice", 6000);
                        match invoice {
                            Ok(inv) => println!("User Invoice: {}", inv),
                            Err(e) => println!("Error creating invoice: {}", e)
                        }
                    } else {
                        println!("Invalid sats value provided");
                    }
                },
                (Some("payinvoice"), [invoice_str]) => {
                    let bolt11_invoice = invoice_str.parse::<Bolt11Invoice>();
                    match bolt11_invoice {
                        Ok(invoice) => {
                            match user.bolt11_payment().send(&invoice) {
                                Ok(payment_id) => println!("Payment sent from User with payment_id: {}", payment_id),
                                Err(e) => println!("Error sending payment from User: {}", e)
                            }
                        },
                        Err(e) => println!("Error parsing invoice: {}", e),
                    }
                },
                (Some("getjitinvoice"), []) => {
                    match user.bolt11_payment().receive_via_jit_channel(
                        16700000, 
                        "Stable Channel", 
                        3600, 
                        Some(10000000000000)
                    ) {
                        Ok(invoice) => println!("Invoice: {:?}", invoice.to_string()),
                        Err(e) => println!("Error: {:?}", e),
                    }
                },
                (Some("exit"), _) => break,
                _ => println!("Unknown command or incorrect arguments:"),
            }
        }
    }

    #[cfg(feature = "lsp")] {
        let lsp = make_hack_node("lsp", 9737);
        let mut their_offer: Option<Offer> = None;

        loop {
            let (input, command, args) = get_user_input("Enter command for lsp: ");

        match (command.as_deref(), args.as_slice()) {
            (Some("settheiroffer"), [their_offer_str]) => {
                their_offer = Some(Offer::from_str(&their_offer_str).unwrap());
                println!("Offer set.");
    
            },
            (Some("getouroffer"),[]) => {
                let our_offer: Offer = lsp.bolt12_payment().receive_variable_amount("thanks").unwrap();
                println!("{}", our_offer);
            },
            (Some("getaddress"), []) => {
                let funding_address = lsp.onchain_payment().new_address();
                match funding_address {
                    Ok(fund_addr) => println!("LSP Funding Address: {}", fund_addr),
                    Err(e) => println!("Error getting funding address: {}", e),
                }
            },
            // (Some("openchannel"), args) => {
            //     if args.len() != 3 {
            //         println!("Error: 'openchannel' command requires three parameters: <node_id>, <listening_address>, and <sats>");
            //         return;
            //     }

            //     let node_id_str = &args[0];
            //     let listening_address_str = &args[1];
            //     let sats_str = &args[2];

            //     let user_node_id = node_id_str.parse().unwrap();
            //     let lsp_net_address: SocketAddress = listening_address_str.parse().unwrap();
            //     let sats: u64 = sats_str.parse().unwrap();
        
            //     let channel_config: Option<Arc<ChannelConfig>> = None;    
                
            //     let announce_channel = true;

            //     match lsp.connect_open_channel(user_node_id, lsp_net_address, sats, Some(sats/2), channel_config, announce_channel) {
            //         Ok(_) => println!("Channel successfully opened to {}", node_id_str),
            //         Err(e) => println!("Failed to open channel: {}", e),
            //     }
            
            // },
            // Sample start command below:
                // startstablechannel CHANNEL_ID IS_STABLE_RECEIVER EXPECTED_DOLLAR_AMOUNT EXPECTED_BTC_AMOUNT
                // startstablechannel 569b7829b98de19a86ec7d73079a0b3c5e03686aa923e86669f6ab8397674759 false 172.0 0
                (Some("startstablechannel"), [channel_id, is_stable_receiver, expected_dollar_amount, native_amount_sats]) => {
                    let channel_id = channel_id.to_string();
                    let is_stable_receiver = is_stable_receiver.parse::<bool>().unwrap_or(false);
                    let expected_dollar_amount = expected_dollar_amount.parse::<f64>().unwrap_or(0.0);
                    let native_amount_sats = native_amount_sats.parse::<f64>().unwrap_or(0.0);

                    let counterparty = lsp.list_channels()
                        .iter()
                        .find(|channel| {
                            println!("channel_id: {}", channel.channel_id);
                            channel.channel_id.to_string() == channel_id
                        })
                        .map(|channel| channel.counterparty_node_id)
                        .expect("Failed to find channel with the specified sID");
                
                    let channel_id_bytes: [u8; 32] = hex::decode(channel_id)
                        .expect("Invalid hex string")
                        .try_into()
                        .expect("Decoded channel ID has incorrect length");

                    let mut stable_channel = StableChannel {
                        channel_id: ChannelId::from_bytes(channel_id_bytes),
                        is_stable_receiver,  
                        counterparty,
                        expected_usd: USD::from_f64(expected_dollar_amount),
                        expected_btc: Bitcoin::from_btc(native_amount_sats),
                        stable_receiver_btc: Bitcoin::from_btc(0.0),
                        stable_provider_btc: Bitcoin::from_btc(0.0),  
                        stable_receiver_usd: USD::from_f64(0.0),
                        stable_provider_usd: USD::from_f64(0.0),
                        risk_level: 0, 
                        timestamp: 0,
                        formatted_datetime: "2021-06-01 12:00:00".to_string(), 
                        payment_made: false,
                        sc_dir: "/path/to/sc_dir".to_string(),
                        latest_price: 0.0, 
                        prices: "".to_string()                    };

                    println!("Stable Channel created: {:?}", stable_channel.channel_id.to_string());

                    loop {
                        let now = SystemTime::now();
                        let now_duration = now.duration_since(UNIX_EPOCH).unwrap();
                    
                        let now_secs = now_duration.as_secs();
                    
                        let next_60_sec = ((now_secs / 60) + 1) * 60;
                        let next_60_sec_duration = Duration::from_secs(next_60_sec);
                    
                        let sleep_duration = next_60_sec_duration
                            .checked_sub(now_duration)
                            .unwrap_or_else(|| Duration::from_secs(0));
                    
                        std::thread::sleep(sleep_duration);
                                            println!();
                        println!(
                            "\nChecking stability for channel {}...\n",
                            stable_channel.channel_id
                        );
                        
                        stable_channel = check_stability(&lsp, stable_channel);
                    }
                },
            (Some("balance"), []) => {
                let balances = lsp.list_balances();
                let onchain_balance = Bitcoin::from_sats(balances.total_onchain_balance_sats);
                let lightning_balance = Bitcoin::from_sats(balances.total_lightning_balance_sats);
                println!("LSP On-Chain Balance: {}", onchain_balance);
                println!("LSP Lightning Balance: {}", lightning_balance);
                // println!("All: {}", balances.);
            },
            (Some("listchannels"), []) => {
                println!("{}", "channels:");
                for channel in lsp.list_channels().iter() {
                    let channel_id = channel.channel_id;
                    println!("{}", channel_id);
                }
                println!("{}", "channel details:");
                let channels = lsp.list_channels();
                println!("{:#?}", channels);
            },
            (Some("listallchannels"), []) => {
                let channels = lsp.list_channels();
                if channels.is_empty() {
                    println!("No channels found.");
                } else {
                    println!("User Channels:");
                    for channel in channels.iter() {
                        println!("--------------------------------------------");
                        println!("Channel ID: {}", channel.channel_id);
                        println!("Channel Value: {}", Bitcoin::from_sats(channel.channel_value_sats));
                        // println!("Our Balance: {}", Bitcoin::from_sats(channel.outbound_capacity_msat / 1000));
                        // println!("Their Balance: {}", Bitcoin::from_sats(channel.inbound_capacity_msat / 1000));
                        println!("Channel Ready?: {}", channel.is_channel_ready);
                    }
                    println!("--------------------------------------------");
                }
            },
            (Some("getinvoice"), [sats]) => {
                if let Ok(sats_value) = sats.parse::<u64>() {
                    let msats = sats_value * 1000;
                    let bolt11 = lsp.bolt11_payment();
                    let invoice = bolt11.receive(msats, "test invoice", 3600);
                    match invoice {
                        Ok(inv) => println!("LSP Invoice: {}", inv),
                        Err(e) => println!("Error creating invoice: {}", e)
                    }
                } else {
                    println!("Invalid sats value provided");
                }
            },
            (Some("payinvoice"), [invoice_str]) => {
                let bolt11_invoice = invoice_str.parse::<Bolt11Invoice>();
                match bolt11_invoice {
                    Ok(invoice) => {
                        match lsp.bolt11_payment().send(&invoice) {
                            Ok(payment_id) => println!("Payment sent from LSP with payment_id: {}", payment_id),
                            Err(e) => println!("Error sending payment from LSP: {}", e)
                        }
                    },
                    Err(e) => println!("Error parsing invoice: {}", e),
                }
            },
            (Some("onchainsend"), [address_str]) => {
                match Address::from_str(address_str) {
                    Ok(addr) => match addr.require_network(lsp.config().network) {
                        Ok(addr_checked) => {
                            match lsp.onchain_payment().send_all_to_address(&addr_checked) {
                                Ok(txid) => println!("{}", txid),
                                Err(e) => eprintln!("Error: {}", e),
                            }
                        }
                        Err(_) => eprintln!("Invalid address for network"),
                    }
                    Err(_) => eprintln!("Invalid address"),
                }                
            },
            (Some("exit"), _) => break,
            _ => println!("Unknown command or incorrect arguments: {}", input),
            // lsp.payment.onchain_payment.send_all_to_address("bc1pe2yt0wtdsyuuagaz2dpc0v0084gqfryx860t5d32qsxdjg84tc0qqu29r8",false);
            
         




        }
    
    }
}   

}

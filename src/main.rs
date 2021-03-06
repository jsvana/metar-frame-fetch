use std::cmp::Ordering;
use std::collections::HashMap;
use std::convert::{TryFrom, TryInto};
use std::fmt;
use std::time::Duration;

use anyhow::{format_err, Context, Result};
use futures::future::join_all;
use itertools::Itertools;
use log::{debug, error, info};
use maplit::hashmap;
use structopt::StructOpt;

#[derive(Debug, StructOpt)]
#[structopt(
    name = "metar_fetch",
    about = "Fetches METAR data and sends serial commands to a photo frame"
)]
struct Args {
    /// Serial port for connected Pro Micro
    #[structopt(long, default_value = "/dev/ttyACM0")]
    serial_port: String,

    /// Baud rate for serial link to Pro Micro
    #[structopt(long, default_value = "9600")]
    baud_rate: u32,

    /// Timeout for serial writes to Pro Micro in milliseconds
    #[structopt(long, default_value = "500")]
    serial_timeout_ms: u64,

    /// Interval to refresh METAR data in seconds
    #[structopt(long, default_value = "300")]
    refresh_interval_s: u64,
}

#[derive(Clone, Debug, PartialEq)]
enum FlightRules {
    LowIfr,
    Ifr,
    MarginalVfr,
    Vfr,
}

impl PartialOrd for FlightRules {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(match (self, other) {
            (FlightRules::LowIfr, FlightRules::LowIfr) => Ordering::Equal,
            (FlightRules::LowIfr, FlightRules::Ifr) => Ordering::Less,
            (FlightRules::LowIfr, FlightRules::MarginalVfr) => Ordering::Less,
            (FlightRules::LowIfr, FlightRules::Vfr) => Ordering::Less,
            (FlightRules::Ifr, FlightRules::LowIfr) => Ordering::Greater,
            (FlightRules::Ifr, FlightRules::Ifr) => Ordering::Equal,
            (FlightRules::Ifr, FlightRules::MarginalVfr) => Ordering::Less,
            (FlightRules::Ifr, FlightRules::Vfr) => Ordering::Less,
            (FlightRules::MarginalVfr, FlightRules::LowIfr) => Ordering::Greater,
            (FlightRules::MarginalVfr, FlightRules::Ifr) => Ordering::Greater,
            (FlightRules::MarginalVfr, FlightRules::MarginalVfr) => Ordering::Equal,
            (FlightRules::MarginalVfr, FlightRules::Vfr) => Ordering::Less,
            (FlightRules::Vfr, FlightRules::LowIfr) => Ordering::Greater,
            (FlightRules::Vfr, FlightRules::Ifr) => Ordering::Greater,
            (FlightRules::Vfr, FlightRules::MarginalVfr) => Ordering::Greater,
            (FlightRules::Vfr, FlightRules::Vfr) => Ordering::Equal,
        })
    }
}

impl TryFrom<&metar::Visibility> for FlightRules {
    type Error = anyhow::Error;

    fn try_from(visibility: &metar::Visibility) -> Result<Self, Self::Error> {
        if visibility.unit != metar::DistanceUnit::StatuteMiles {
            return Err(format_err!("unsupported visibility distance unit"));
        }

        if visibility.visibility < 1.0 {
            Ok(FlightRules::LowIfr)
        } else if visibility.visibility < 3.0 {
            Ok(FlightRules::Ifr)
        } else if visibility.visibility <= 5.0 {
            Ok(FlightRules::MarginalVfr)
        } else {
            Ok(FlightRules::Vfr)
        }
    }
}

impl From<&Vec<metar::CloudLayer>> for FlightRules {
    fn from(layers: &Vec<metar::CloudLayer>) -> Self {
        if layers.is_empty() {
            return FlightRules::Vfr;
        }

        let mut ceiling_altitudes = Vec::new();

        for layer in layers.iter() {
            // TODO(jsvana): handle ceilings with unspecified altitudes
            if let metar::CloudLayer::Broken(_, Some(altitude))
            | metar::CloudLayer::Overcast(_, Some(altitude)) = layer
            {
                ceiling_altitudes.push(altitude);
            }
        }

        match ceiling_altitudes.into_iter().min() {
            Some(altitude) => {
                if *altitude < 5 {
                    FlightRules::LowIfr
                } else if *altitude < 10 {
                    FlightRules::Ifr
                } else if *altitude <= 30 {
                    FlightRules::MarginalVfr
                } else {
                    FlightRules::Vfr
                }
            }
            None => FlightRules::Vfr,
        }
    }
}

impl TryFrom<&metar::Metar<'_>> for FlightRules {
    type Error = anyhow::Error;

    fn try_from(m: &metar::Metar) -> Result<Self, Self::Error> {
        match (&m.visibility, &m.cloud_layers) {
            (metar::Data::Known(visibility), cloud_layers) => {
                let visibility_flight_rules: FlightRules = visibility.try_into()?;
                let cloud_layers_flight_rules: FlightRules = cloud_layers.into();

                if visibility_flight_rules < cloud_layers_flight_rules {
                    Ok(visibility_flight_rules)
                } else {
                    Ok(cloud_layers_flight_rules)
                }
            }
            (metar::Data::Unknown, _) => {
                return Err(format_err!("missing visibility"));
            }
        }
    }
}

#[derive(Debug)]
enum FlightRulesColor {
    Purple,
    Red,
    Blue,
    Green,
}

impl From<FlightRules> for FlightRulesColor {
    fn from(rules: FlightRules) -> Self {
        match rules {
            FlightRules::LowIfr => FlightRulesColor::Purple,
            FlightRules::Ifr => FlightRulesColor::Red,
            FlightRules::MarginalVfr => FlightRulesColor::Blue,
            FlightRules::Vfr => FlightRulesColor::Green,
        }
    }
}

impl fmt::Display for FlightRulesColor {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(
            f,
            "{}",
            match self {
                FlightRulesColor::Purple => "p",
                FlightRulesColor::Red => "r",
                FlightRulesColor::Blue => "b",
                FlightRulesColor::Green => "g",
            }
        )
    }
}

struct ColorAndPort {
    color: FlightRulesColor,
    port: u16,
}

async fn flight_rules_color_for_airport(airport: &str, port: u16) -> Result<ColorAndPort> {
    let res = reqwest::get(&format!(
        "https://tgftp.nws.noaa.gov/data/observations/metar/stations/{}.TXT",
        airport
    ))
    .await
    .with_context(|| format_err!("failed to fetch METAR for {}", airport))?;

    let body = res.text().await.with_context(|| {
        format_err!(
            "failed to get HTTP response text for METAR request for {}",
            airport
        )
    })?;

    let mut lines = body.lines();
    lines.next();

    let r = metar::Metar::parse(
        &lines
            .next()
            .ok_or_else(|| format_err!("missing METAR line for {}", airport))?,
    )
    .map_err(|e| format_err!("failed to parse METAR for {}: {}", airport, e))?;

    let rules: FlightRules = (&r)
        .try_into()
        .with_context(|| format_err!("failed to parse METAR into flight rules for {}", airport))?;

    debug!(
        "{} is {:?} ({})",
        airport,
        &rules,
        FlightRulesColor::from(rules.clone()),
    );

    Ok(ColorAndPort {
        color: rules.into(),
        port,
    })
}

async fn set_colors(
    serial_port: &str,
    baud_rate: u32,
    serial_port_timeout: Duration,
    port_map: &HashMap<&str, u16>,
) -> Result<()> {
    let mut futures = Vec::new();
    for (airport, port) in port_map {
        futures.push(flight_rules_color_for_airport(airport, *port));
    }

    let mut port = serialport::new(serial_port, baud_rate)
        .timeout(serial_port_timeout)
        .open()
        .context("failed to open serial device")?;

    for result in join_all(futures).await {
        let color_and_port = result.context("failed to fetch flight rules")?;

        port.write(&format!("{}{}", color_and_port.port, color_and_port.color).as_bytes())
            .context("failed to write flight rules to microcontroller")?;
    }

    Ok(())
}

#[tokio::main]
async fn main() -> ! {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("warn")).init();

    let args = Args::from_args();
    let serial_port_timeout = Duration::from_millis(args.serial_timeout_ms);

    let port_map: HashMap<&str, u16> = hashmap! {
        "KOAK" => 1,
        "KSFO" => 2,
        "KHAF" => 3,
        "KSQL" => 4,
        "KSJC" => 5,
    };

    for (airport, port) in port_map.iter().sorted_by_key(|i| i.1) {
        info!("{} on LED{}", airport, port);
    }

    let mut timer = tokio::time::interval(Duration::from_secs(args.refresh_interval_s));
    timer.tick().await;

    loop {
        info!("Querying METARs and setting colors");

        if let Err(e) = set_colors(
            &args.serial_port,
            args.baud_rate,
            serial_port_timeout.clone(),
            &port_map,
        )
        .await
        {
            error!("failed to set colors: {:?}", e);
        }

        timer.tick().await;
    }
}

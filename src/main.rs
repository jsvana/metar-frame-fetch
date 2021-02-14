use std::cmp::Ordering;
use std::convert::{TryFrom, TryInto};

use anyhow::{format_err, Result};
use maplit::hashmap;

#[derive(Debug, PartialEq)]
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

#[tokio::main]
async fn main() -> Result<()> {
    let port_map = hashmap! {
        //"KSFO" => 1,
        //"KSQL" => 2,
        //"KPAO" => 3,
        "KSJC" => 4,
        "KVUO" => 4,
        "KHIO" => 4,
        "KOLM" => 4,
        "KGRF" => 4,
        "KPLU" => 4,
    };

    for (airport, port) in port_map {
        let res = reqwest::get(&format!(
            "https://tgftp.nws.noaa.gov/data/observations/metar/stations/{}.TXT",
            airport
        ))
        .await?;

        let body = res.text().await?;

        let mut lines = body.lines();
        lines.next();

        let r = metar::Metar::parse(
            &lines
                .next()
                .ok_or_else(|| format_err!("missing METAR line"))?,
        )
        .map_err(|e| format_err!("failed to parse METAR: {}", e))?;

        let flight_rules: FlightRules = (&r).try_into()?;

        println!("{}: {:?}", airport, flight_rules);
    }

    Ok(())
}

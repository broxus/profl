use std::collections::{HashMap, HashSet};
use std::fmt::{Display, Formatter};
use std::io::{BufRead, BufReader, Read};
use std::path::PathBuf;

use anyhow::{Context, Result};
use argh::FromArgs;
use bincode::Options;
use itertools::{Itertools, MinMaxResult};
use plotters::prelude::*;
use serde::{Deserialize, Serialize};
use smallstr::SmallString;

#[derive(FromArgs)]
/// Logs plotter
struct Args {
    /// path to log file
    #[argh(option)]
    path: String,
    #[argh(option)]
    /// filter expr
    filter: Option<String>,
    #[argh(option)]
    /// output svg file name
    output: Option<String>,
}


#[derive(Serialize, Deserialize, Debug)]
struct Log {
    took: u64,
    context: String,
    path: String,
}

#[derive(Serialize, Deserialize)]
pub struct Record {
    x: Option<u64>,
    /// NOTE: 500 years should be enough
    y: u64,
    id: SmallString<[u8; 32]>,
    path: SmallString<[u8; 64]>,
}

fn logs_stream<P: AsRef<std::path::Path>>(path: P, filter: Option<String>) -> Result<Vec<Record>> {
    let filter = match filter {
        None => None,
        Some(a) => {
            let names: HashSet<String> = a.split(',').map(|x| x.to_string()).collect();
            Some(move |x: &str| {
                names.contains(x)
            })
        }
    };

    let file = std::fs::OpenOptions::new().read(true).open(path)?;
    let mut reader = BufReader::new(file);
    let mut res = Vec::with_capacity(1024);
    let mut all = HashMap::new();

    loop {
        let data =
            match read_segment(&mut reader) {
                Ok(a) => a,
                Err(e) => {
                    if reader.fill_buf()?.is_empty() {
                        break;
                    }
                    println!("Failed deserializing: {:#?}", e);
                    break;
                }
            };
        *all.entry(data.id.clone()).or_insert(0) += 1;
        if let Some(ref filter) = filter {
            if filter(&data.id) {
                res.push(data);
            }
        } else { res.push(data) }
    }

    println!("All ids:");
    for (id, count) in all.into_iter().sorted_by(|x, y| y.1.cmp(&x.1)) {
        println!("{}: {}", id, count);
    }

    Ok(res)
}

fn read_segment<R>(bytes: R) -> Result<Record> where R: Read {
    bincode::options().deserialize_from(bytes).context("Failed deserilzing from")
}

fn main() -> Result<()> {
    let args: Args = argh::from_env();
    let path = PathBuf::from(&args.path);
    let output = args.output.as_ref().map(PathBuf::from).unwrap_or_else(|| PathBuf::from("plot.svg"));
    let data = logs_stream(&path, args.filter)?;
    plot(data, output)?;
    Ok(())
}


fn plot(data: Vec<Record>, out: PathBuf) -> Result<()> {
    let map = data.into_iter().into_group_map_by(|x| x.id.clone()); //todo bench me
    let len = map.len();
    let root = SVGBackend::new(&out, (1920, 600 * len as u32)).into_drawing_area();

    for ((id, records), cell) in map.into_iter().zip(root.split_evenly((len, 1))) {
        let records: Vec<_> = records.into_iter().map(|x| x.y).collect();

        println!("Histogram for {}", id);
        let mut hist = hdrhistogram::Histogram::new(1)?;
        records.iter().for_each(|x| hist.record(*x).unwrap());
        let (min, max, mean) = (hist.min(), hist.max(), hist.mean());
        let description = histogram_print(hist);
        println!("Min: {}, Max: {}", min, max);

        let len = records.len() as u64;
        let (label, div) = Time(mean as u64).get_div_and_postfix();

        let data = records.iter().copied().map(|y| y / div);
        let (min, max) = match data.clone().minmax() {
            MinMaxResult::MinMax(min, max) => { (min, max) }
            _ => anyhow::bail!("No data"),
        };

        cell.fill(&WHITE)?;

        let mut ctx = ChartBuilder::on(&cell)
            .set_label_area_size(LabelAreaPosition::Left, 80)
            .set_label_area_size(LabelAreaPosition::Bottom, 80)
            .caption(format!("{}\n{}", id, description), ("sans-serif", 30))
            .build_cartesian_2d(0..len, min..max)?;

        ctx.configure_mesh()
            .y_label_formatter(&|y| format!("{:.0} {}", *y, label))
            .draw()?;


        ctx.draw_series(AreaSeries::new(
            data.enumerate().map(|(x, y)| (x as u64, y)), // The data iter
            0,                                  // Baseline
            &RED.mix(0.2), // Make the series opac
        ).border_style(&RED) // Make a brighter border)?;
        )?;
    }
    root.present()?;
    Ok(())
}


fn histogram_print(h: hdrhistogram::Histogram<u64>) -> String {
    {
        println!("Distribution");
        for v in break_once(
            h.iter_quantiles(10),
            |v| v.quantile() > 0.95,
        ) {
            println!(
                "{:4}µs | {:40} | {:4.1}th %-ile",
                (v.value_iterated_to() + 1) / 1_000,
                "*".repeat(
                    (v.count_since_last_iteration() as f64 * 40.0 / h.len() as f64).ceil() as usize
                ),
                v.percentile(),
            );
        };
    }

    // until we have https://github.com/rust-lang/rust/issues/62208
    fn break_once<I, F>(it: I, mut f: F) -> impl Iterator<Item=I::Item>
        where
            I: IntoIterator,
            F: FnMut(&I::Item) -> bool,
    {
        let mut got_true = false;
        it.into_iter().take_while(move |i| {
            if got_true {
                // we've already yielded when f was true
                return false;
            }
            if f(i) {
                // this must be the first time f returns true
                // we should yield i, and then no more
                got_true = true;
            }
            // f returned false, so we should keep yielding
            true
        })
    }

    format!(
        "p50: {}, p90: {}, p99: {}, p999: {}, min: {}, max: {}",
        Time(h.value_at_quantile(0.5)),
        Time(h.value_at_quantile(0.9)),
        Time(h.value_at_quantile(0.99)),
        Time(h.value_at_quantile(0.999)),
        Time(h.min()),
        Time(h.max()),
    )
}

struct Time(u64);

impl Time {
    pub fn get_div_and_postfix(&self) -> (&'static str, u64) {
        let size = (self.0 as f64).log10().floor() as u16;
        match size {
            9..=u16::MAX => ("sec", 1_000_000_000),
            6..=8 => ("millis", 1_000_000),
            3..=5 => ("micros", 1_000),
            _ => ("nanos", 1)
        }
    }
}

impl Display for Time {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        let (label, div) = self.get_div_and_postfix();
        writeln!(f, "{} {}", self.0 / div, label)
    }
}
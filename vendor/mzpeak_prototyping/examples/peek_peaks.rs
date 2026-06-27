use mzpeak_prototyping::MzPeakReader;
use mzdata::prelude::*;
use std::path::PathBuf;
fn main() -> std::io::Result<()> {
    let mut a = std::env::args().skip(1);
    let path = PathBuf::from(a.next().unwrap());
    let mut reader = MzPeakReader::new(&path)?;
    for idx in a.map(|s| s.parse::<usize>().unwrap()) {
        match reader.get_spectrum(idx) {
            None => println!("idx {idx}: None"),
            Some(s) => {
                let lvl = s.ms_level();
                // params of interest
                let c0 = s.description().params().iter().find(|p| p.name.contains("tof_c0")).and_then(|p| p.to_f64().ok());
                let c1 = s.description().params().iter().find(|p| p.name.contains("tof_c1")).and_then(|p| p.to_f64().ok());
                if let Some(pk) = s.peaks.as_ref() {
                    let (mn,mx)=pk.iter().map(|p|p.mz()).fold((f64::MAX,f64::MIN),|(a,b),v|(a.min(v),b.max(v)));
                    println!("idx {idx} ms{lvl} PEAKS n={} m/z[{:.2},{:.2}] c0={:?} c1={:?}", pk.len(), mn, mx, c0, c1);
                } else if let Some(arr) = s.raw_arrays() {
                    let types: Vec<String> = arr.iter().map(|(t,_)| format!("{:?}", t)).collect();
                    match arr.mzs() {
                        Ok(m) if !m.is_empty() => { let (mn,mx)=m.iter().fold((f64::MAX,f64::MIN),|(a,b),&v|(a.min(v),b.max(v)));
                            println!("idx {idx} ms{lvl} RAW n={} m/z[{:.2},{:.2}] arrays={:?} c0={:?} c1={:?}", m.len(), mn, mx, types, c0, c1); }
                        _ => println!("idx {idx} ms{lvl} RAW noMZ arrays={:?} c0={:?} c1={:?}", types, c0, c1),
                    }
                } else { println!("idx {idx} ms{lvl} empty c0={:?} c1={:?}", c0, c1); }
            }
        }
    }
    Ok(())
}

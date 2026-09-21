//! Throwaway: render the strip chart to PNGs so it can be looked at without a display.
use std::f64::consts::PI;

// Pulled in only to switch on cairo-rs's `png` feature for this example.
use cairo as _;

// Only `draw` and `Sample` are used here; ChartView and the Sample constructors
// belong to the GUI, and are dead only in this compilation unit.
#[path = "../src/chart.rs"]
#[allow(dead_code)]
mod chart;

fn session() -> Vec<chart::Sample> {
    // A plausible gaming session: idle, a load ramp, power pinned at PL1, temperatures
    // climbing behind it, the fan chasing them, and a few throttle events at the peak.
    (0..900)
        .map(|i| {
            let t = f64::from(i);
            let phase = (t / 900.0).clamp(0.0, 1.0);
            let load = if t < 60.0 {
                8.0
            } else {
                72.0 + 22.0 * (t / 40.0).sin()
            };
            let gpu = if t < 60.0 {
                3.0
            } else {
                88.0 + 9.0 * (t / 25.0).cos()
            };
            let heat = if t < 60.0 {
                0.0
            } else {
                1.0 - (-(t - 60.0) / 180.0).exp()
            };
            let power = if t < 60.0 {
                3.2
            } else {
                35.0 - 1.6 * (t / 30.0).sin().abs()
            };
            let cpu_c = 44.0 + heat * 46.0 + 3.0 * (t / 18.0 + PI).sin();
            chart::Sample {
                t,
                cpu_pct: Some(load.clamp(0.0, 100.0)),
                gpu_pct: Some(gpu.clamp(0.0, 100.0)),
                mem_pct: Some(41.0 + phase * 22.0),
                package_w: Some(power),
                // Rails as a plausible share of the package: cores take the bulk under
                // CPU load, the iGPU tracks its own curve, and the two leave a
                // remainder, as on the real machine.
                cpu_w: Some(power * 0.62),
                gpu_w: Some(power * 0.22 * (0.4 + gpu.clamp(0.0, 100.0) / 140.0)),
                pl1_w: Some(35.0),
                cpu_c: Some(cpu_c),
                battery_c: Some(33.0 + heat * 9.0),
                board_c: Some(38.0 + heat * 28.0),
                fan_rpm: Some(if t < 60.0 { 0.0 } else { 900.0 + heat * 4300.0 }),
                // Only at the top of the ramp, and only sometimes.
                throttled: cpu_c > 88.0 && (i % 37 == 0),
            }
        })
        .collect()
}

fn render(name: &str, dark: bool, samples: Vec<chart::Sample>) {
    let (w, h) = (900, chart::natural_height());
    let surface =
        gtk::cairo::ImageSurface::create(gtk::cairo::Format::ARgb32, w, h).expect("surface");
    let cr = gtk::cairo::Context::new(&surface).expect("context");
    // The chart draws no background of its own; it sits on the window's surface.
    if dark {
        cr.set_source_rgb(0.141, 0.141, 0.141);
    } else {
        cr.set_source_rgb(0.98, 0.98, 0.98);
    }
    let _ = cr.paint();

    let data = chart::ChartData {
        samples,
        placeholder: "nothing to plot".into(),
    };
    chart::draw(&cr, w, h, &data, chart::Palette::for_theme(dark));
    drop(cr);

    let mut file = std::fs::File::create(name).expect("create");
    surface.write_to_png(&mut file).expect("png");
    println!("wrote {name}");
}

fn main() {
    let dir = std::env::args().nth(1).unwrap_or_else(|| ".".into());
    render(&format!("{dir}/chart-light.png"), false, session());
    render(&format!("{dir}/chart-dark.png"), true, session());
    // The live case: a short window, sparse enough that no downsampling happens.
    render(
        &format!("{dir}/chart-live.png"),
        false,
        session().into_iter().take(90).collect(),
    );
}

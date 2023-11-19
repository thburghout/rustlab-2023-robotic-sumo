#![no_std]
#![no_main]
#![feature(type_alias_impl_trait)]
#![feature(async_fn_in_trait)]
#![allow(incomplete_features)]

// 192.168.88.12
// 255.255.255.0
// 192.168.88.1

use cyw43_pio::PioSpi;
use embassy_executor::Spawner;
use embassy_rp::adc::{Adc, Channel, Config as ConfigAdc, InterruptHandler as InterruptHandlerAdc};
use embassy_net::{Config as ConfigNet, Stack, StackResources, StaticConfigV4};
use embassy_rp::bind_interrupts;
use embassy_rp::gpio::{Input, Level, Output, Pull};
use embassy_rp::peripherals::{DMA_CH0, PIN_23, PIN_25, PIO0, USB};
use embassy_rp::pio::{InterruptHandler as InterruptHandlerPio, Pio};
use embassy_rp::usb::{Driver, InterruptHandler as InterruptHandlerUsb};
use embassy_rp::pwm::{Config as PwmConfig, Pwm};
use embassy_time::{Duration, Timer};
use fixed::prelude::ToFixed;
use rp2040_panic_usb_boot as _;
use smoltcp::wire::Ipv4Cidr;
use static_cell::make_static;

bind_interrupts!(struct Irqs {
    USBCTRL_IRQ => InterruptHandlerUsb<USB>;
    ADC_IRQ_FIFO => InterruptHandlerAdc;
    PIO0_IRQ_0 => InterruptHandlerPio<PIO0>;
});

#[embassy_executor::task]
async fn logger_task(driver: Driver<'static, USB>) {
    embassy_usb_logger::run!(1024, log::LevelFilter::Info, driver);
}

const WIFI_SSID: &'static str = include_str!("WIFI_SSID.txt");
const WIFI_SECRET: &'static str = include_str!("WIFI_SECRET.txt");


const PWN_DIV_INT: u8 = 250;
const PWM_TOP: u16 = 10000;

fn pwm_config(duty_a: u16, duty_b: u16) -> PwmConfig {
    let mut c = PwmConfig::default();
    c.invert_a = false;
    c.invert_b = false;
    c.phase_correct = false;
    c.enable = true;
    c.divider = PWN_DIV_INT.to_fixed();
    c.compare_a = duty_a;
    c.compare_b = duty_b;
    c.top = PWM_TOP;
    c
}

const MAX_DUTY: u16 = 3500;

#[embassy_executor::main]
async fn main(spawner: Spawner) {
    let p = embassy_rp::init(Default::default());

    // Init USB logger
    let driver = Driver::new(p.USB, Irqs);
    spawner.spawn(logger_task(driver)).unwrap();


    // Init input pins
    let gp0 = Input::new(p.PIN_0, Pull::None);
    let gp1 = Input::new(p.PIN_1, Pull::None);

    // init Analog pin
    let mut adc = Adc::new(p.ADC, Irqs, ConfigAdc::default());
    let mut gp26 = Channel::new_pin(p.PIN_26, Pull::None);

    // wheels
    let mut pwm_rhs = Pwm::new_output_ab(p.PWM_CH1, p.PIN_2, p.PIN_3, pwm_config(0, 0));
    let mut pwm_lhs = Pwm::new_output_ab(p.PWM_CH3, p.PIN_6, p.PIN_7, pwm_config(0, 0));

    // Use cyw43 firmware
    let fw = include_bytes!("../deps/cyw43-firmware/43439A0.bin");
    let clm = include_bytes!("../deps/cyw43-firmware/43439A0_clm.bin");

    // Init cyw43
    let pwr = Output::new(p.PIN_23, Level::Low);
    let cs = Output::new(p.PIN_25, Level::High);
    let mut pio = Pio::new(p.PIO0, Irqs);
    let spi = PioSpi::new(
        &mut pio.common,
        pio.sm0,
        pio.irq0,
        cs,
        p.PIN_24,
        p.PIN_29,
        p.DMA_CH0,
    );
    let state = make_static!(cyw43::State::new());
    let (net_device, mut control, runner) = cyw43::new(state, pwr, spi, fw).await;
    spawner.spawn(wifi_task(runner)).unwrap();
    control.init(clm).await;
    control
        .set_power_management(cyw43::PowerManagementMode::PowerSave)
        .await;


    // Generate random seed
    let seed = 0x0123_4567_89ab_cd42; // chosen by fair dice roll. guarenteed to be random.


    // Init network stack
    let config = ConfigNet::ipv4_static(StaticConfigV4 {
        address: Ipv4Cidr::new("192.168.88.12".parse().unwrap(), 24),
        gateway: Some("192.168.88.1".parse().unwrap()),
        dns_servers: Default::default(),
    });
    let stack = &*make_static!(Stack::new(
        net_device,
        config,
        make_static!(StackResources::<2>::new()),
        seed
    ));
    spawner.spawn(net_task(stack)).unwrap();

    // Join wifi network
    log::info!(
        "Joining access point {} (link up: {})",
        WIFI_SSID,
        stack.is_link_up()
    );

    loop {
        match control.join_wpa2(WIFI_SSID.trim(), WIFI_SECRET.trim()).await {
            Ok(_) => break,
            Err(err) => {
                log::info!("join failed with status={}", err.status);
            }
        }
    }

    // Use PWM pins
    let mut counter = 0;
    loop {
        log::info!("Link is up: {}", stack.is_link_up());
        let gp0_level = gp0.get_level();
        let gp1_level = gp1.get_level();
        let gp26 = adc.read(&mut gp26).await.unwrap();
        log::info!(
            "GP0: {:?}, GP1: {:?}, GP26: {}",
            gp0_level,
            gp1_level,
            gp26,

        );

        let (duty_a, duty_b) = match counter % 4 {
            1 => (MAX_DUTY, 0),
            2 => (0, MAX_DUTY),
            // 3 => (MAX_DUTY, MAX_DUTY),
            _ => (0, 0),
        };

        log::info!("A: {}, B: {}", duty_a, duty_b);

        let c1 = pwm_config(duty_a, duty_b);
        let c2 = pwm_config(duty_a, duty_b);

        pwm_rhs.set_config(&c1);
        pwm_lhs.set_config(&c2);

        counter = (counter + 1) % 4;

        Timer::after(Duration::from_millis(500)).await;
    }
}


#[embassy_executor::task]
async fn wifi_task(
    runner: cyw43::Runner<
        'static,
        Output<'static, PIN_23>,
        PioSpi<'static, PIN_25, PIO0, 0, DMA_CH0>,
    >,
) -> ! {
    runner.run().await
}


#[embassy_executor::task]
async fn net_task(stack: &'static Stack<cyw43::NetDriver<'static>>) -> ! {
    stack.run().await
}

// #[embassy_executor::task]
// async fn network_task() {
//
// }

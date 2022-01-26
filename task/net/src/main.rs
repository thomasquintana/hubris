// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

#![no_std]
#![no_main]

mod buf;
mod server;

use core::sync::atomic::{AtomicU32, Ordering};
use stm32h7::stm32h743 as device;

use drv_stm32h7_eth as eth;
use drv_stm32h7_rcc_api::Rcc;
use userlib::*;

task_slot!(RCC, rcc_driver);
task_slot!(GPIO, gpio_driver);

/////////////////////////////////////////////////////////////////////////////
// Configuration things!
//
// Much of this needs to move into the board-level configuration.

/// Address used on the MDIO link by our Ethernet PHY. Different vendors have
/// different defaults for this, it will likely need to become configurable.
const PHYADDR: u8 = 0x01;

static FAKE_MAC: [u8; 6] = [0x02, 0x04, 0x06, 0x08, 0x0A, 0x0C];

const TX_RING_SZ: usize = 4;

const RX_RING_SZ: usize = 4;

/// Notification mask for our IRQ; must match configuration in app.toml.
const ETH_IRQ: u32 = 1;

/// Number of entries to maintain in our neighbor cache (ARP/NDP).
const NEIGHBORS: usize = 4;

/////////////////////////////////////////////////////////////////////////////
// Main driver loop.

/// Global count of passes through the driver loop, for inspection through a
/// debugger.
static ITER_COUNT: AtomicU32 = AtomicU32::new(0);

#[export_name = "main"]
fn main() -> ! {
    let rcc = RCC.get_task_id();
    let rcc = Rcc::from(rcc);

    // Turn on the Ethernet power.
    rcc.enable_clock(drv_stm32h7_rcc_api::Peripheral::Eth1Rx);
    rcc.enable_clock(drv_stm32h7_rcc_api::Peripheral::Eth1Tx);
    rcc.enable_clock(drv_stm32h7_rcc_api::Peripheral::Eth1Mac);

    // Reset the MAC. This is one of two resets that must occur for the MAC to
    // work; the other is below.
    rcc.enter_reset(drv_stm32h7_rcc_api::Peripheral::Eth1Mac);
    rcc.leave_reset(drv_stm32h7_rcc_api::Peripheral::Eth1Mac);

    configure_ethernet_pins();

    // Set up our ring buffers.
    let (tx_storage, tx_buffers) = buf::claim_tx_statics();
    let tx_ring = eth::ring::TxRing::new(tx_storage, tx_buffers);
    let (rx_storage, rx_buffers) = buf::claim_rx_statics();
    let rx_ring = eth::ring::RxRing::new(rx_storage, rx_buffers);

    // Create the driver instance.
    let eth = eth::Ethernet::new(
        unsafe { &*device::ETHERNET_MAC::ptr() },
        unsafe { &*device::ETHERNET_MTL::ptr() },
        unsafe { &*device::ETHERNET_DMA::ptr() },
        tx_ring,
        rx_ring,
    );

    // Set up the network stack.

    use smoltcp::iface::Neighbor;
    use smoltcp::socket::UdpSocket;
    use smoltcp::wire::{EthernetAddress, IpAddress};

    let mac = EthernetAddress::from_bytes(&FAKE_MAC);

    let ipv6_addr = link_local_iface_addr(mac);
    let ipv6_net = smoltcp::wire::Ipv6Cidr::new(ipv6_addr, 64).into();

    let mut ip_addrs = [ipv6_net];
    let mut neighbor_cache_storage: [Option<(IpAddress, Neighbor)>; NEIGHBORS] =
        [None; NEIGHBORS];
    let neighbor_cache =
        smoltcp::iface::NeighborCache::new(&mut neighbor_cache_storage[..]);

    let mut socket_storage =
        [smoltcp::iface::SocketStorage::EMPTY; generated::SOCKET_COUNT];
    let mut eth =
        smoltcp::iface::InterfaceBuilder::new(eth, &mut socket_storage[..])
            .hardware_addr(mac.into())
            .neighbor_cache(neighbor_cache)
            .ip_addrs(&mut ip_addrs[..])
            .finalize();

    // Create sockets and associate them with the interface.
    let sockets = generated::construct_sockets();
    let mut socket_handles = [None; generated::SOCKET_COUNT];
    for (socket, h) in sockets.0.into_iter().zip(&mut socket_handles) {
        *h = Some(eth.add_socket(socket));
    }
    let socket_handles = socket_handles.map(|h| h.unwrap());
    // Bind sockets to their ports.
    for (&h, &port) in socket_handles.iter().zip(&generated::SOCKET_PORTS) {
        eth.get_socket::<UdpSocket>(h)
            .bind(port)
            .map_err(|_| ())
            .unwrap();
    }

    // Set up the PHY.
    let mii_basic_control = eth
        .device_mut()
        .smi_read(PHYADDR, eth::SmiClause22Register::Control);
    let mii_basic_control = mii_basic_control
        | 1 << 12 // AN enable
        | 1 << 9 // restart autoneg
        ;
    eth.device_mut().smi_write(
        PHYADDR,
        eth::SmiClause22Register::Control,
        mii_basic_control,
    );

    // Wait for link-up
    while eth
        .device_mut()
        .smi_read(PHYADDR, eth::SmiClause22Register::Status)
        & (1 << 2)
        == 0
    {
        userlib::hl::sleep_for(1);
    }

    // Turn on our IRQ.
    userlib::sys_irq_control(ETH_IRQ, true);

    // Move resources into the server impl.
    let mut server = server::ServerImpl::new(socket_handles, eth);

    // Go!
    loop {
        ITER_COUNT.fetch_add(1, Ordering::Relaxed);

        // Call into smoltcp.
        let poll_result =
            server
                .interface_mut()
                .poll(smoltcp::time::Instant::from_millis(
                    userlib::sys_get_timer().now as i64,
                ));

        let any_activity = poll_result.unwrap_or(true);

        if any_activity {
            // There's something to do. Iterate over sockets looking for work.
            // TODO making every packet O(n) in the number of sockets is super
            // lame; provide a Waker to fix this.
            for i in 0..generated::SOCKET_COUNT {
                if server.get_socket_mut(i).unwrap().can_recv() {
                    // Make sure the owner knows about this. This can
                    // technically cause spurious wakeups if the owner is
                    // already waiting in our incoming queue to recv. Maybe we
                    // fix this later.
                    let (task_id, notification) = generated::SOCKET_OWNERS[i];
                    let task_id = sys_refresh_task_id(task_id);
                    sys_post(task_id, notification);
                }
            }
        } else {
            // No work to do immediately. Wait for an ethernet IRQ or an
            // incoming message.
            let mut msgbuf = [0u8; server::ServerImpl::INCOMING_SIZE];
            idol_runtime::dispatch_n(&mut msgbuf, &mut server);
        }
    }
}

/// We can map an Ethernet MAC address into the IPv6 space as follows.
///
/// - The top 64 bits are `fe80::`, putting it in the link-local (non-routable)
///   address space.
/// - The bottom 64 bits are the Interface ID, which we generate with the EUI-64
///   method.
///
/// The EUI-64 transform for a MAC address is given in RFC4291 section 2.5.1,
/// and can be summarized as follows.
///
/// - Insert the bytes `FF FE` in the middle to extend the MAC address to 8
///   bytes.
/// - Flip bit 1 in the first byte, to translate the OUI universal/local bit
///   into the IPv6 universal/local bit.
fn link_local_iface_addr(
    mac: smoltcp::wire::EthernetAddress,
) -> smoltcp::wire::Ipv6Address {
    let mut bytes = [0; 16];
    // Link-local address block.
    bytes[0..2].copy_from_slice(&[0xFE, 0x80]);
    // Bytes 2..8 are all zero.
    // Top three bytes of MAC address...
    bytes[8..11].copy_from_slice(&mac.0[0..3]);
    // ...with administration scope bit flipped.
    bytes[8] ^= 0b0000_0010;
    // Inserted FF FE from EUI64 transform.
    bytes[11..13].copy_from_slice(&[0xFF, 0xFE]);
    // Bottom three bytes of MAC address.
    bytes[13..16].copy_from_slice(&mac.0[3..6]);

    smoltcp::wire::Ipv6Address(bytes)
}

fn configure_ethernet_pins() {
    // TODO this mapping is hard-coded for the STM32H7 Nucleo board!
    //
    // This board's mapping:
    //
    // RMII REF CLK     PA1
    // MDIO             PA2
    // RMII RX DV       PA7
    //
    // MDC              PC1
    // RMII RXD0        PC4
    // RMII RXD1        PC5
    //
    // RMII TX EN       PG11
    // RMII TXD1        PB13 <-- port B
    // RMII TXD0        PG13
    use drv_stm32h7_gpio_api::*;

    let gpio = Gpio::from(GPIO.get_task_id());
    let eth_af = Alternate::AF11;

    gpio.configure(
        Port::A,
        (1 << 1) | (1 << 2) | (1 << 7),
        Mode::Alternate,
        OutputType::PushPull,
        Speed::VeryHigh,
        Pull::None,
        eth_af,
    )
    .unwrap();
    gpio.configure(
        Port::B,
        1 << 13,
        Mode::Alternate,
        OutputType::PushPull,
        Speed::VeryHigh,
        Pull::None,
        eth_af,
    )
    .unwrap();
    gpio.configure(
        Port::C,
        (1 << 1) | (1 << 4) | (1 << 5),
        Mode::Alternate,
        OutputType::PushPull,
        Speed::VeryHigh,
        Pull::None,
        eth_af,
    )
    .unwrap();
    gpio.configure(
        Port::G,
        (1 << 11) | (1 << 12) | (1 << 13),
        Mode::Alternate,
        OutputType::PushPull,
        Speed::VeryHigh,
        Pull::None,
        eth_af,
    )
    .unwrap();
}

// Place to namespace all the bits generated by our config processor.
mod generated {
    include!(concat!(env!("OUT_DIR"), "/net_config.rs"));
}
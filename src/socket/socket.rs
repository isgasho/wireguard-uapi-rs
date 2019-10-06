use super::{link_message, WireGuardDeviceLinkOperation};
use crate::attr::WgDeviceAttribute;
use crate::cmd::WgCmd;
use crate::consts::{WG_GENL_NAME, WG_GENL_VERSION};
use crate::err::{ConnectError, GetDeviceError, LinkDeviceError, SetDeviceError};
use crate::get;
use crate::set;
use crate::set::create_set_device_messages;
use crate::socket::parse::*;
use crate::socket::NlWgMsgType;
use crate::DeviceInterface;
use libc::IFNAMSIZ;
use neli::consts::{NlFamily, NlmF, Nlmsg};
use neli::genl::Genlmsghdr;
use neli::nl::Nlmsghdr;
use neli::nlattr::Nlattr;
use neli::socket::NlSocket;
use neli::Nl;
use neli::StreamWriteBuffer;

pub struct Socket {
    sock: NlSocket,
    family_id: NlWgMsgType,
}

impl Socket {
    pub fn connect() -> Result<Self, ConnectError> {
        let family_id = {
            NlSocket::new(NlFamily::Generic, true)?
                .resolve_genl_family(WG_GENL_NAME)
                .map_err(ConnectError::ResolveFamilyError)?
        };

        let track_seq = true;
        let mut wgsock = NlSocket::new(NlFamily::Generic, track_seq)?;

        // Autoselect a PID
        let pid = None;
        let groups = None;
        wgsock.bind(pid, groups)?;

        Ok(Self {
            sock: wgsock,
            family_id,
        })
    }

    pub fn get_device(
        &mut self,
        interface: DeviceInterface,
    ) -> Result<get::Device, GetDeviceError> {
        let mut mem = StreamWriteBuffer::new_growable(None);
        let attr = match interface {
            DeviceInterface::Name(name) => {
                Some(name.len())
                    .filter(|&len| 0 < len && len < IFNAMSIZ)
                    .ok_or_else(|| GetDeviceError::InvalidInterfaceName)?;
                name.as_ref().serialize(&mut mem)?;
                Nlattr::new(None, WgDeviceAttribute::Ifname, mem.as_ref())?
            }
            DeviceInterface::Index(index) => {
                index.serialize(&mut mem)?;
                Nlattr::new(None, WgDeviceAttribute::Ifindex, mem.as_ref())?
            }
        };
        let genlhdr = {
            let cmd = WgCmd::GetDevice;
            let version = WG_GENL_VERSION;
            let attrs = vec![attr];
            Genlmsghdr::new(cmd, version, attrs)?
        };
        let nlhdr = {
            let size = None;
            let nl_type = self.family_id;
            let flags = vec![NlmF::Request, NlmF::Ack, NlmF::Dump];
            let seq = None;
            let pid = None;
            let payload = genlhdr;
            Nlmsghdr::new(size, nl_type, flags, seq, pid, payload)
        };

        self.sock.send_nl(nlhdr)?;

        // In the future, neli will return multiple Netlink messages. We have to go through each
        // message and coalesce peers in the way described by the WireGuard UAPI when this change
        // happens. For now, parsing is broken if the entire response doesn't fit in a single
        // payload.
        //
        // See: https://github.com/jbaublitz/neli/issues/15

        let mut iter = self
            .sock
            .iter::<Nlmsg, Genlmsghdr<WgCmd, WgDeviceAttribute>>();

        let mut device = None;
        while let Some(Ok(response)) = iter.next() {
            match response.nl_type {
                Nlmsg::Error => return Err(GetDeviceError::AccessError),
                Nlmsg::Done => break,
                _ => (),
            };

            let handle = response.nl_payload.get_attr_handle();
            device = Some(match device {
                Some(device) => extend_device(device, handle)?,
                None => parse_device(handle)?,
            });
        }

        device.ok_or(GetDeviceError::AccessError)
    }

    pub fn set_device(&mut self, device: set::Device) -> Result<(), SetDeviceError> {
        for nl_message in create_set_device_messages(device, self.family_id)? {
            self.sock.send_nl(nl_message)?;
            self.sock.recv_ack()?;
        }

        Ok(())
    }

    pub fn list_device_names(&self) -> Result<Vec<String>, failure::Error> {
        use neli::consts::{Arphrd, Ifla, Rtm};
        use neli::rtnl::Ifinfomsg;
        use neli::rtnl::Rtattr;

        let infomsg = {
            let ifi_family =
                neli::consts::rtnl::RtAddrFamily::UnrecognizedVariant(libc::AF_UNSPEC as u8);
            // Arphrd::Netrom corresponds to 0. Not sure why 0 is necessary here but this is what the
            // embedded C library does.
            let ifi_type = Arphrd::Netrom;
            let ifi_index = 0;
            let ifi_flags = vec![];
            let rtattrs: Vec<Rtattr<Ifla, Vec<u8>>> = vec![];
            Ifinfomsg::new(ifi_family, ifi_type, ifi_index, ifi_flags, rtattrs)
        };

        let nlmsg = {
            let len = None;
            let nl_type = Rtm::Getlink;
            let flags = vec![NlmF::Request, NlmF::Ack, NlmF::Dump];
            let seq = None;
            let pid = None;
            let payload = infomsg;
            Nlmsghdr::new(len, nl_type, flags, seq, pid, payload)
        };

        let mut sock = NlSocket::connect(NlFamily::Route, None, None, true)?;
        sock.send_nl(nlmsg)?;

        let mut iter = sock.iter::<Nlmsg, Ifinfomsg<Ifla>>();

        while let Some(Ok(response)) = iter.next() {
            println!("new link:");
            match response.nl_type {
                Nlmsg::Error => panic!("err"),
                Nlmsg::Done => break,
                _ => (),
            };

            let attrs = response.nl_payload.rtattrs;
            for attr in attrs {
                match attr.rta_type {
                    // Ifla::UnrecognizedVariant(IFLA_LINKINFO) => {
                    //     println!("Hello! {:#?}", attr.rta_payload)
                    // }
                    Ifla::Ifname => println!("name: {:#?}", String::from_utf8(attr.rta_payload)?),
                    _ => {}
                };
            }
        }

        Ok(vec![])
    }

    pub fn add_device(&self, ifname: &str) -> Result<(), LinkDeviceError> {
        let mut sock = NlSocket::connect(NlFamily::Route, None, None, true)?;
        sock.send_nl(link_message(ifname, WireGuardDeviceLinkOperation::Add)?)?;
        sock.recv_ack()?;
        Ok(())
    }

    pub fn del_device(&self, ifname: &str) -> Result<(), LinkDeviceError> {
        let mut sock = NlSocket::connect(NlFamily::Route, None, None, true)?;
        sock.send_nl(link_message(ifname, WireGuardDeviceLinkOperation::Delete)?)?;
        sock.recv_ack()?;
        Ok(())
    }
}

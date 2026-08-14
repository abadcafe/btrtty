use anyhow::{Context, Result, bail};
use objc2::Message;
use objc2::msg_send;
use objc2::rc::Retained;
use objc2_foundation::{NSMutableArray, NSMutableDictionary, NSNumber, NSString, ns_string};
use objc2_io_bluetooth::{IOBluetoothSDPServiceRecord, IOBluetoothSDPUUID};

const SERIAL_PORT_UUID: u16 = 0x1101;
const L2CAP_UUID: u16 = 0x0100;
const RFCOMM_UUID: u16 = 0x0003;
const PUBLIC_BROWSE_GROUP_UUID: u16 = 0x1002;
const SPP_VERSION_1_2: u16 = 0x0102;

pub(super) struct PublishedService {
    pub(super) record: Retained<IOBluetoothSDPServiceRecord>,
    pub(super) channel_id: u8,
}

pub(super) fn publish() -> Result<PublishedService> {
    let dictionary = build_service_dictionary()?;
    // SAFETY: The dictionary follows IOBluetoothSDPDataElement's documented
    // representation and remains alive for the duration of this call.
    let record = unsafe {
        IOBluetoothSDPServiceRecord::publishedServiceRecordWithDictionary(Some(&dictionary))
    }
    .context("IOBluetooth rejected the SPP service dictionary")?;

    let mut channel_id = 0_u8;
    // SAFETY: `channel_id` is a valid out pointer for the duration of the call.
    let status = unsafe { record.getRFCOMMChannelID(std::ptr::from_mut(&mut channel_id)) };
    if status != 0 || channel_id == 0 {
        // SAFETY: The record was just published by this process.
        unsafe { record.removeServiceRecord() };
        bail!("read dynamically assigned RFCOMM channel: IOReturn {status:#x}");
    }

    Ok(PublishedService { record, channel_id })
}

pub(super) fn build_service_dictionary() -> Result<Retained<NSMutableDictionary>> {
    // SAFETY: Each UUID is a valid Bluetooth-assigned 16-bit UUID.
    let serial_port = unsafe { IOBluetoothSDPUUID::uuid16(SERIAL_PORT_UUID) }
        .context("create SerialPort SDP UUID")?;
    // SAFETY: Each UUID is a valid Bluetooth-assigned 16-bit UUID.
    let l2cap =
        unsafe { IOBluetoothSDPUUID::uuid16(L2CAP_UUID) }.context("create L2CAP SDP UUID")?;
    // SAFETY: Each UUID is a valid Bluetooth-assigned 16-bit UUID.
    let rfcomm =
        unsafe { IOBluetoothSDPUUID::uuid16(RFCOMM_UUID) }.context("create RFCOMM SDP UUID")?;
    // SAFETY: Each UUID is a valid Bluetooth-assigned 16-bit UUID.
    let public_browse = unsafe { IOBluetoothSDPUUID::uuid16(PUBLIC_BROWSE_GROUP_UUID) }
        .context("create PublicBrowseGroup SDP UUID")?;

    let service_classes = array();
    add(&service_classes, &*serial_port);

    let l2cap_descriptor = array();
    add(&l2cap_descriptor, &*l2cap);
    let rfcomm_descriptor = array();
    add(&rfcomm_descriptor, &*rfcomm);
    let dynamic_channel = unsigned_data_element(1, 0);
    add(&rfcomm_descriptor, &*dynamic_channel);
    let protocols = array();
    add(&protocols, &*l2cap_descriptor);
    add(&protocols, &*rfcomm_descriptor);

    let browse_groups = array();
    add(&browse_groups, &*public_browse);

    let profile = array();
    add(&profile, &*serial_port);
    let profile_version = unsigned_data_element(2, SPP_VERSION_1_2.into());
    add(&profile, &*profile_version);
    let profiles = array();
    add(&profiles, &*profile);

    let language = array();
    for value in [0x656e_u32, 0x006a, 0x0100] {
        let element = unsigned_data_element(2, value);
        add(&language, &*element);
    }

    let dictionary = NSMutableDictionary::new();
    insert(
        &dictionary,
        ns_string!("0001 - ServiceClassIDList"),
        &*service_classes,
    );
    insert(
        &dictionary,
        ns_string!("0004 - ProtocolDescriptorList"),
        &*protocols,
    );
    insert(
        &dictionary,
        ns_string!("0005 - BrowseGroupList"),
        &*browse_groups,
    );
    insert(
        &dictionary,
        ns_string!("0006 - LanguageBaseAttributeIDList"),
        &*language,
    );
    insert(
        &dictionary,
        ns_string!("0009 - BluetoothProfileDescriptorList"),
        &*profiles,
    );
    insert(
        &dictionary,
        ns_string!("0100 - ServiceName"),
        ns_string!("Btrtty Terminal"),
    );
    Ok(dictionary)
}

fn array() -> Retained<NSMutableArray> {
    NSMutableArray::new()
}

fn unsigned_data_element(size: u8, value: u32) -> Retained<NSMutableDictionary> {
    let dictionary = NSMutableDictionary::new();
    let element_type = NSNumber::numberWithUnsignedChar(1);
    let element_size = NSNumber::numberWithUnsignedChar(size);
    let element_value = NSNumber::numberWithUnsignedInt(value);
    insert(&dictionary, ns_string!("DataElementType"), &*element_type);
    insert(&dictionary, ns_string!("DataElementSize"), &*element_size);
    insert(&dictionary, ns_string!("DataElementValue"), &*element_value);
    dictionary
}

fn add<T: Message>(array: &NSMutableArray, object: &T) {
    // SAFETY: NSMutableArray accepts any non-null Objective-C object and
    // retains it. `T: Message` guarantees an Objective-C object pointer.
    unsafe {
        let _: () = msg_send![array, addObject: object];
    }
}

fn insert<T: Message>(dictionary: &NSMutableDictionary, key: &NSString, object: &T) {
    // SAFETY: The key is an immutable NSString (and therefore NSCopying), and
    // both key and value are valid Objective-C objects retained by the map.
    unsafe {
        let _: () = msg_send![dictionary, setObject: object, forKey: key];
    }
}

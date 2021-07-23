use bitbuffer::{BitRead, BitReadSized, BitWrite, BitWriteSized, BitWriteStream, LittleEndian};
use serde::{Deserialize, Serialize};
use serde_repr::{Deserialize_repr, Serialize_repr};

use crate::demo::message::stringtable::log_base2;
use crate::demo::packet::datatable::{ClassId, SendTable};
use crate::demo::parser::{Encode, ParseBitSkip};
use crate::demo::sendprop::{SendProp, SendPropIdentifier, SendPropValue};
use crate::{Parse, ParseError, ParserState, ReadResult, Result, Stream};
use parse_display::{Display, FromStr};
use std::cmp::min;

use std::fmt;
use std::hint::unreachable_unchecked;
use std::num::NonZeroU32;

#[derive(
    Debug,
    Copy,
    Clone,
    PartialEq,
    Eq,
    Hash,
    Serialize,
    Deserialize,
    Display,
    Ord,
    PartialOrd,
    FromStr,
    Default,
)]
pub struct EntityId(u32);

impl From<u32> for EntityId {
    fn from(num: u32) -> Self {
        EntityId(num)
    }
}

#[derive(
    BitRead, BitWrite, Clone, Copy, Debug, PartialEq, Eq, Serialize_repr, Deserialize_repr,
)]
#[discriminant_bits = 2]
#[repr(u8)]
pub enum PVS {
    Preserve = 0,
    Leave = 1,
    Enter = 2,
    Delete = 3,
}

#[derive(Debug, Clone, PartialEq)]
pub struct PacketEntity {
    pub server_class: ClassId,
    pub entity_index: EntityId,
    pub props: Vec<SendProp>,
    pub in_pvs: bool,
    pub pvs: PVS,
    pub serial_number: u32,
    pub delay: Option<u32>,
}

impl fmt::Display for PacketEntity {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(f, "{}({}) {{", self.entity_index, self.server_class)?;
        for child in self.props.iter() {
            writeln!(f, "\t{}", child)?;
        }
        write!(f, "}}")
    }
}

impl PacketEntity {
    pub fn mut_prop_by_identifier(&mut self, index: &SendPropIdentifier) -> Option<&mut SendProp> {
        self.props.iter_mut().find(|prop| prop.identifier == *index)
    }

    pub fn get_prop_by_identifier(&self, index: &SendPropIdentifier) -> Option<&SendProp> {
        self.props.iter().find(|prop| prop.identifier == *index)
    }

    pub fn apply_update(&mut self, props: Vec<SendProp>) {
        for prop in props {
            match self.mut_prop_by_identifier(&prop.identifier) {
                Some(existing_prop) => existing_prop.value = prop.value,
                None => self.props.push(prop),
            }
        }
    }

    pub fn get_prop_by_name(&self, table_name: &str, name: &str) -> Option<&SendProp> {
        let identifier = SendPropIdentifier::new(table_name, name);
        self.get_prop_by_identifier(&identifier)
    }

    pub fn diff_from_baseline<'a>(
        &'a self,
        baseline: &'a [SendProp],
    ) -> impl Iterator<Item = &'a SendProp> + 'a {
        // self.props.iter().filter(move |prop| {
        //     !baseline
        //         .iter()
        //         .any(|base_prop| base_prop.index == prop.index && base_prop.value == prop.value)
        // })
        self.props.iter().filter(move |prop| {
            baseline
                .iter()
                .find(|base_prop| base_prop.identifier == prop.identifier)
                .map(|base_prop| base_prop.value != prop.value)
                .unwrap_or(true)
        })
    }
}

fn read_bit_var<'a, T: BitReadSized<'a, LittleEndian>>(stream: &mut Stream<'a>) -> ReadResult<T> {
    let ty: u8 = stream.read_sized(2)?;

    let bits = match ty {
        0 => 4,
        1 => 8,
        2 => 12,
        3 => 32,
        _ => unsafe { unreachable_unchecked() },
    };
    stream.read_sized(bits)
}

fn write_bit_var(var: u32, stream: &mut BitWriteStream<LittleEndian>) -> ReadResult<()> {
    let ty: u8 = if var >= 2u32.pow(12) {
        3
    } else if var >= 2u32.pow(8) {
        2
    } else if var >= 2u32.pow(4) {
        1
    } else {
        0
    };
    ty.write_sized(stream, 2)?;

    let bits = match ty {
        0 => 4,
        1 => 8,
        2 => 12,
        3 => 32,
        _ => unsafe { unreachable_unchecked() },
    };

    var.write_sized(stream, bits)
}

#[test]
fn test_bit_var_roundtrip() {
    use bitbuffer::{BitReadBuffer, BitReadStream};

    fn bit_var_normal(val: u32) {
        let mut data = Vec::with_capacity(16);
        let pos = {
            let mut write = BitWriteStream::new(&mut data, LittleEndian);
            write_bit_var(val, &mut write).unwrap();
            write.bit_len()
        };
        let mut read = BitReadStream::new(BitReadBuffer::new(&data, LittleEndian));
        assert_eq!(val, read_bit_var(&mut read).unwrap());
        assert_eq!(pos, read.pos());
    }
    bit_var_normal(0);
    bit_var_normal(1);
    bit_var_normal(24);
    bit_var_normal(1234);
    bit_var_normal(12345);
    bit_var_normal(123456);
    bit_var_normal(1234567);
    bit_var_normal(12345678);
    bit_var_normal(123456789);
}

#[derive(Debug, PartialEq)]
pub struct PacketEntitiesMessage {
    pub entities: Vec<PacketEntity>,
    pub removed_entities: Vec<EntityId>,
    pub max_entries: u16,
    pub delta: Option<NonZeroU32>,
    pub base_line: u8,
    pub updated_base_line: bool,
}

fn get_send_table(state: &ParserState, class: ClassId) -> Result<&SendTable> {
    state
        .send_tables
        .get(usize::from(class))
        .ok_or(ParseError::UnknownServerClass(class))
}

fn get_entity_for_update(
    state: &ParserState,
    entity_index: EntityId,
    pvs: PVS,
) -> Result<PacketEntity> {
    let class_id = *state
        .entity_classes
        .get(&entity_index)
        .ok_or(ParseError::UnknownEntity(entity_index))?;

    Ok(PacketEntity {
        server_class: class_id,
        entity_index,
        props: Vec::with_capacity(8),
        in_pvs: false,
        pvs,
        serial_number: 0,
        delay: None,
    })
}

impl Parse<'_> for PacketEntitiesMessage {
    fn parse(stream: &mut Stream, state: &ParserState) -> Result<Self> {
        let max_entries = stream.read_sized(11)?;
        let delta: Option<u32> = stream.read()?;
        let base_line = stream.read_sized(1)?;
        let updated_entries: u16 = stream.read_sized(11)?;
        let length: u32 = stream.read_sized(20)?;
        let updated_base_line = stream.read()?;

        let mut data = stream.read_bits(length as usize)?;

        let mut entities = Vec::with_capacity(min(updated_entries, 128) as usize);
        let mut removed_entities = Vec::new();

        let mut last_index: i32 = -1;

        for _ in 0..updated_entries {
            let diff: u32 = read_bit_var(&mut data)?;
            last_index = last_index.saturating_add(diff as i32).saturating_add(1);
            let entity_index = EntityId::from(last_index as u32);

            let pvs = data.read()?;
            if pvs == PVS::Enter {
                let mut entity =
                    Self::read_enter(&mut data, entity_index, state, base_line as usize)?;
                let send_table = get_send_table(state, entity.server_class)?;
                Self::read_update(&mut data, send_table, &mut entity.props)?;

                entities.push(entity);
            } else if pvs == PVS::Preserve {
                let mut entity = get_entity_for_update(state, entity_index, pvs)?;
                let send_table = get_send_table(state, entity.server_class)?;

                Self::read_update(&mut data, send_table, &mut entity.props)?;

                entities.push(entity);
            } else if state.entity_classes.contains_key(&entity_index) {
                let entity = get_entity_for_update(state, entity_index, pvs)?;
                entities.push(entity);
            }
        }

        if delta.is_some() {
            while data.read()? {
                removed_entities.push(data.read_sized::<u32>(11)?.into())
            }
        }

        Ok(PacketEntitiesMessage {
            entities,
            removed_entities,
            max_entries,
            delta: delta.and_then(NonZeroU32::new),
            base_line,
            updated_base_line,
        })
    }
}

impl Encode for PacketEntitiesMessage {
    fn encode(&self, stream: &mut BitWriteStream<LittleEndian>, state: &ParserState) -> Result<()> {
        self.max_entries.write_sized(stream, 11)?;
        self.delta.is_some().write(stream)?;
        if let Some(delta) = self.delta {
            delta.get().write(stream)?;
        }
        self.base_line.write_sized(stream, 1)?;
        self.entities.len().write_sized(stream, 11)?;

        stream.reserve(20, |length_stream, stream| {
            self.updated_base_line.write(stream)?;

            let length_start = stream.bit_len();

            let mut last_index: i32 = -1;

            for entity in self.entities.iter() {
                let diff = entity.entity_index.0 as i32 - last_index - 1;
                write_bit_var(diff as u32, stream)?;
                last_index = entity.entity_index.0 as i32;

                entity.pvs.write(stream)?;

                let send_table = get_send_table(state, entity.server_class)?;
                match entity.pvs {
                    PVS::Enter => {
                        Self::write_enter(entity, stream, state)?;
                        let baseline = state.get_baseline(
                            self.base_line as usize,
                            entity.entity_index,
                            entity.server_class,
                            send_table,
                        )?;
                        Self::write_update(
                            entity.props.iter().skip(baseline.len()),
                            stream,
                            send_table,
                        )?;
                    }
                    PVS::Preserve => {
                        Self::write_update(&entity.props, stream, send_table)?;
                    }
                    _ => {}
                }
            }

            if self.delta.is_some() {
                for removed in self.removed_entities.iter() {
                    true.write(stream)?;
                    removed.0.write_sized(stream, 11)?;
                }
                false.write(stream)?;
            }

            let length_end = stream.bit_len();

            (length_end - length_start).write_sized(length_stream, 20)?;

            Ok(())
        })
    }
}

impl PacketEntitiesMessage {
    fn read_enter(
        stream: &mut Stream,
        entity_index: EntityId,
        state: &ParserState,
        baseline_index: usize,
    ) -> Result<PacketEntity> {
        let bits = log_base2(state.server_classes.len()) + 1;
        let class_index: ClassId = stream.read_sized::<u16>(bits as usize)?.into();

        let serial = stream.read_sized(10)?;
        let send_table = state
            .send_tables
            .get(usize::from(class_index))
            .ok_or(ParseError::UnknownServerClass(class_index))?;

        let props = state.get_baseline(baseline_index, entity_index, class_index, send_table)?;

        Ok(PacketEntity {
            server_class: class_index,
            entity_index,
            props,
            in_pvs: true,
            pvs: PVS::Enter,
            serial_number: serial,
            delay: None,
        })
    }

    fn write_enter(
        entity: &PacketEntity,
        stream: &mut BitWriteStream<LittleEndian>,
        state: &ParserState,
    ) -> Result<()> {
        let bits = log_base2(state.server_classes.len()) + 1;
        let (class_index, _class) = state
            .server_classes
            .iter()
            .enumerate()
            .find(|(_, class)| entity.server_class == class.id)
            .ok_or(ParseError::UnknownServerClass(entity.server_class))?;
        class_index.write_sized(stream, bits as usize)?;
        entity.serial_number.write_sized(stream, 10)?;

        Ok(())
    }

    pub fn read_update(
        stream: &mut Stream,
        send_table: &SendTable,
        props: &mut Vec<SendProp>,
    ) -> Result<()> {
        let mut index: i32 = -1;

        while stream.read()? {
            let diff: u32 = read_bit_var(stream)?;
            index = index.saturating_add(diff as i32).saturating_add(1);

            match send_table.flattened_props.get(index as usize) {
                Some(definition) => {
                    let value = SendPropValue::parse(stream, &definition.parse_definition)?;
                    props.push(SendProp {
                        index: index as u32,
                        identifier: definition.identifier,
                        value,
                    });
                }
                None => {
                    return Err(ParseError::PropIndexOutOfBounds {
                        index,
                        prop_count: send_table.flattened_props.len(),
                    });
                }
            }
        }

        Ok(())
    }

    fn write_update<'a, Props: IntoIterator<Item = &'a SendProp>>(
        props: Props,
        stream: &mut BitWriteStream<LittleEndian>,
        send_table: &SendTable,
    ) -> Result<()> {
        let mut last_index: i32 = -1;

        for prop in props {
            true.write(stream)?;

            let index = prop.index as usize;
            let definition = send_table
                .flattened_props
                .get(index)
                .ok_or(ParseError::UnknownDefinition(prop.identifier))?;
            write_bit_var((index as i32 - last_index - 1) as u32, stream)?;
            last_index = index as i32;
            prop.value.encode(stream, &definition.parse_definition)?;
        }
        false.write(stream)?;
        Ok(())
    }
}

impl ParseBitSkip<'_> for PacketEntitiesMessage {
    fn parse_skip(stream: &mut Stream, _state: &ParserState) -> Result<()> {
        stream.skip_bits(11)?;
        if stream.read()? {
            stream.skip_bits(32)?;
        }
        stream.skip_bits(12)?;
        let length: u32 = stream.read_sized(20)?;
        stream
            .skip_bits(length as usize + 1)
            .map_err(ParseError::from)
    }
}

#[test]
fn test_packet_entitier_message_roundtrip() {
    use crate::demo::packet::datatable::{SendTable, SendTableName, ServerClass, ServerClassName};
    use crate::demo::sendprop::{FloatDefinition, SendPropDefinition, SendPropParseDefinition};

    let mut state = ParserState::new(24, |_| false, false);
    state.server_classes = vec![
        ServerClass {
            id: ClassId::from(0),
            name: ServerClassName::from("class1"),
            data_table: SendTableName::from("table1"),
        },
        ServerClass {
            id: ClassId::from(1),
            name: ServerClassName::from("class2"),
            data_table: SendTableName::from("table2"),
        },
    ];
    state.send_tables = vec![
        SendTable {
            name: SendTableName::from("table1"),
            needs_decoder: false,
            raw_props: vec![],
            flattened_props: vec![],
        },
        SendTable {
            name: SendTableName::from("table2"),
            needs_decoder: false,
            raw_props: vec![],
            flattened_props: vec![
                SendPropDefinition {
                    identifier: SendPropIdentifier::new("table2", "prop1"),
                    parse_definition: SendPropParseDefinition::Int {
                        changes_often: false,
                        bit_count: 8,
                    },
                },
                SendPropDefinition {
                    identifier: SendPropIdentifier::new("table2", "prop2"),
                    parse_definition: SendPropParseDefinition::String {
                        changes_often: false,
                    },
                },
                SendPropDefinition {
                    identifier: SendPropIdentifier::new("table2", "prop3"),
                    parse_definition: SendPropParseDefinition::Float {
                        changes_often: false,
                        definition: FloatDefinition::Coord,
                    },
                },
            ],
        },
    ];
    state
        .entity_classes
        .insert(EntityId::from(4), ClassId::from(1));
    crate::test_roundtrip_encode(
        PacketEntitiesMessage {
            entities: vec![],
            removed_entities: vec![],
            max_entries: 0,
            delta: None,
            base_line: 0,
            updated_base_line: false,
        },
        &state,
    );
    crate::test_roundtrip_encode(
        PacketEntitiesMessage {
            entities: vec![PacketEntity {
                server_class: ClassId::from(0),
                entity_index: Default::default(),
                props: vec![],
                in_pvs: true,
                pvs: PVS::Enter,
                serial_number: 0,
                delay: None,
            }],
            removed_entities: vec![],
            max_entries: 4,
            delta: None,
            base_line: 0,
            updated_base_line: false,
        },
        &state,
    );
    crate::test_roundtrip_encode(
        PacketEntitiesMessage {
            entities: vec![
                PacketEntity {
                    server_class: ClassId::from(0),
                    entity_index: EntityId::from(0),
                    props: vec![],
                    in_pvs: true,
                    pvs: PVS::Enter,
                    serial_number: 0,
                    delay: None,
                },
                PacketEntity {
                    server_class: ClassId::from(1),
                    entity_index: EntityId::from(4),
                    props: vec![
                        SendProp {
                            index: 0,
                            identifier: SendPropIdentifier::new("table2", "prop1"),
                            value: SendPropValue::Integer(4),
                        },
                        SendProp {
                            index: 2,
                            identifier: SendPropIdentifier::new("table2", "prop3"),
                            value: SendPropValue::Float(1.0),
                        },
                    ],
                    in_pvs: false,
                    pvs: PVS::Preserve,
                    serial_number: 0,
                    delay: None,
                },
                PacketEntity {
                    server_class: ClassId::from(1),
                    entity_index: EntityId::from(5),
                    props: vec![
                        SendProp {
                            index: 0,
                            identifier: SendPropIdentifier::new("table2", "prop1"),
                            value: SendPropValue::Integer(4),
                        },
                        SendProp {
                            index: 2,
                            identifier: SendPropIdentifier::new("table2", "prop3"),
                            value: SendPropValue::Float(1.0),
                        },
                    ],
                    in_pvs: true,
                    pvs: PVS::Enter,
                    serial_number: 0,
                    delay: None,
                },
            ],
            removed_entities: vec![],
            max_entries: 4,
            delta: None,
            base_line: 0,
            updated_base_line: false,
        },
        &state,
    );
}

//! Deals entirely with schema analysis for the purpose of creating output structs + members
use crate::{OutputMember, OutputStruct};
use anyhow::{bail, Result};
use k8s_openapi::apiextensions_apiserver::pkg::apis::apiextensions::v1::{
    JSONSchemaProps, JSONSchemaPropsOrArray, JSONSchemaPropsOrBool,
};
use std::collections::{BTreeMap, HashMap};

const IGNORED_KEYS: [&str; 3] = ["metadata", "apiVersion", "kind"];

/// Scan a schema for structs and members, and recurse to find all structs
///
/// schema: root schema / sub schema
/// current: current key name (or empty string for first call) - must capitalize first letter
/// stack: stacked concat of kind + current_{n-1} + ... + current (used to create dedup names/types)
/// level: recursion level (start at 0)
/// results: multable list of generated structs (not deduplicated)
pub fn analyze(
    schema: JSONSchemaProps,
    current: &str,
    stack: &str,
    level: u8,
    results: &mut Vec<OutputStruct>,
) -> Result<()> {
    let props = schema.properties.clone().unwrap_or_default();
    let mut array_recurse_level: HashMap<String, u8> = Default::default();
    // first generate the object if it is one
    let current_type = schema.type_.clone().unwrap_or_default();
    if current_type == "object" {
        // we can have additionalProperties XOR properties
        // https://kubernetes.io/docs/tasks/extend-kubernetes/custom-resources/custom-resource-definitions/#validation
        if let Some(JSONSchemaPropsOrBool::Schema(s)) = schema.additional_properties.as_ref() {
            let dict_type = s.type_.clone().unwrap_or_default();
            // object with additionalProperties == map
            if let Some(extra_props) = &s.properties {
                // map values is an object with properties
                debug!("Generating map struct for {} (under {})", current, stack);
                let new_result =
                    analyze_object_properties(&extra_props, stack, &mut array_recurse_level, level, &schema)?;
                results.extend(new_result);
            } else if !dict_type.is_empty() {
                warn!("not generating type {} - using {} map", current, dict_type);
                return Ok(()); // no members here - it'll be inlined
            }
        } else {
            // else, regular properties only
            debug!("Generating struct for {} (under {})", current, stack);
            // initial analysis of properties (we do not recurse here, we need to find members first)
            if props.is_empty() && schema.x_kubernetes_preserve_unknown_fields.unwrap_or(false) {
                warn!("not generating type {} - using BTreeMap", current);
                return Ok(());
            }
            let new_result =
                analyze_object_properties(&props, stack, &mut array_recurse_level, level, &schema)?;
            results.extend(new_result);
        }
    }

    // Start recursion for properties
    for (key, value) in props {
        if level == 0 && IGNORED_KEYS.contains(&(key.as_ref())) {
            debug!("not recursing into ignored {}", key); // handled elsewhere
            continue;
        }
        let next_key = uppercase_first_letter(&key);
        let next_stack = format!("{}{}", stack, next_key);
        let value_type = value.type_.clone().unwrap_or_default();
        match value_type.as_ref() {
            "object" => {
                // objects, maps
                let mut handled_inner = false;
                if let Some(JSONSchemaPropsOrBool::Schema(s)) = &value.additional_properties {
                    let dict_type = s.type_.clone().unwrap_or_default();
                    if dict_type == "array" {
                        // unpack the inner object from the array wrap
                        if let Some(JSONSchemaPropsOrArray::Schema(items)) = &s.as_ref().items {
                            analyze(*items.clone(), &next_key, &next_stack, level + 1, results)?;
                            handled_inner = true;
                        }
                    }
                    // TODO: not sure if these nested recurses are necessary - cluster test case does not have enough data
                    //if let Some(extra_props) = &s.properties {
                    //    for (_key, value) in extra_props {
                    //        debug!("nested recurse into {} {} - key: {}", next_key, next_stack, _key);
                    //        analyze(value.clone(), &next_key, &next_stack, level +1, results)?;
                    //    }
                    //}
                }
                if !handled_inner {
                    // normal object recurse
                    analyze(value, &next_key, &next_stack, level + 1, results)?;
                }
            }
            "array" => {
                if let Some(recurse) = array_recurse_level.get(&key).cloned() {
                    let mut inner = value.clone();
                    for _i in 0..recurse {
                        debug!("recursing into props for {}", key);
                        if let Some(sub) = inner.items {
                            match sub {
                                JSONSchemaPropsOrArray::Schema(s) => {
                                    //info!("got inner: {}", serde_json::to_string_pretty(&s)?);
                                    inner = *s.clone();
                                }
                                _ => bail!("only handling single type in arrays"),
                            }
                        } else {
                            bail!("could not recurse into vec");
                        }
                    }
                    analyze(inner, &next_key, &next_stack, level + 1, results)?;
                }
            }
            "" => {
                if value.x_kubernetes_int_or_string.is_some() {
                    debug!("not recursing into IntOrString {}", key)
                } else {
                    debug!("not recursing into unknown empty type {}", key)
                }
            }
            x => debug!("not recursing into {} (not a container - {})", key, x),
        }
    }
    Ok(())
}

// helper to figure out what output structs (returned) and embedded members are contained in the current object schema
fn analyze_object_properties(
    props: &BTreeMap<String, JSONSchemaProps>,
    stack: &str,
    array_recurse_level: &mut HashMap<String, u8>,
    level: u8,
    schema: &JSONSchemaProps,
) -> Result<Vec<OutputStruct>, anyhow::Error> {
    let mut results = vec![];
    let mut members = vec![];
    let reqs = schema.required.clone().unwrap_or_default();
    for (key, value) in props {
        let value_type = value.type_.clone().unwrap_or_default();
        let rust_type = match value_type.as_ref() {
            "object" => {
                let mut dict_key = None;
                if let Some(additional) = &value.additional_properties {
                    debug!("got additional: {}", serde_json::to_string(&additional)?);
                    if let JSONSchemaPropsOrBool::Schema(s) = additional {
                        // This case is for maps. It is generally String -> Something, depending on the type key:
                        let dict_type = s.type_.clone().unwrap_or_default();
                        dict_key = match dict_type.as_ref() {
                            "string" => Some("String".into()),
                            // We are not 100% sure the array and object subcases here are correct but they pass tests atm.
                            // authoratative, but more detailed sources than crd validation docs below are welcome
                            // https://kubernetes.io/docs/tasks/extend-kubernetes/custom-resources/custom-resource-definitions/#validation
                            "array" => {
                                // agent test with `validationInfo` uses this spec format
                                Some(format!("{}{}", stack, uppercase_first_letter(key)))
                            }
                            "object" => {
                                // cluster test with `failureDomains` uses this spec format
                                Some(format!("{}{}", stack, uppercase_first_letter(key)))
                            }
                            "" => {
                                if s.x_kubernetes_int_or_string.is_some() {
                                    Some("IntOrString".into())
                                } else {
                                    bail!("unknown empty dict type for {}", key)
                                }
                            }
                            // think the type we get is the value type
                            x => Some(uppercase_first_letter(x)), // best guess
                        };
                    }
                } else if value.properties.is_none()
                    && value.x_kubernetes_preserve_unknown_fields.unwrap_or(false)
                {
                    dict_key = Some("serde_json::Value".into());
                }
                if let Some(dict) = dict_key {
                    format!("BTreeMap<String, {}>", dict)
                } else {
                    format!("{}{}", stack, uppercase_first_letter(key))
                }
            }
            "string" => "String".to_string(),
            "boolean" => "bool".to_string(),
            "date" => extract_date_type(value)?,
            "number" => extract_number_type(value)?,
            "integer" => extract_integer_type(value)?,
            "array" => {
                // recurse through repeated arrays until we find a concrete type (keep track of how deep we went)
                let (array_type, recurse_level) = array_recurse_for_type(value, stack, key, 1)?;
                debug!(
                    "got array type {} for {} in level {}",
                    array_type, key, recurse_level
                );
                array_recurse_level.insert(key.clone(), recurse_level);
                array_type
            }
            "" => {
                if value.x_kubernetes_int_or_string.is_some() {
                    "IntOrString".into()
                } else {
                    bail!("unknown empty dict type for {}", key)
                }
            }
            x => bail!("unknown type {}", x),
        };

        // Create member and wrap types correctly
        let member_doc = value.description.clone();
        if reqs.contains(key) {
            debug!("with required member {} of type {}", key, rust_type);
            members.push(OutputMember {
                type_: rust_type,
                name: key.to_string(),
                field_annot: None,
                docs: member_doc,
            })
        } else {
            // option wrapping needed if not required
            debug!("with optional member {} of type {}", key, rust_type);
            members.push(OutputMember {
                type_: format!("Option<{}>", rust_type),
                name: key.to_string(),
                field_annot: Some(r#"#[serde(default, skip_serializing_if = "Option::is_none")]"#.into()),
                docs: member_doc,
            })
        }
    }
    results.push(OutputStruct {
        name: stack.to_string(),
        members,
        level,
        docs: schema.description.clone(),
    });
    Ok(results)
}

// recurse into an array type to find its nested type
// this recursion is intialised and ended within a single step of the outer recursion
fn array_recurse_for_type(
    value: &JSONSchemaProps,
    stack: &str,
    key: &str,
    level: u8,
) -> Result<(String, u8)> {
    if let Some(items) = &value.items {
        match items {
            JSONSchemaPropsOrArray::Schema(s) => {
                let inner_array_type = s.type_.clone().unwrap_or_default();
                return match inner_array_type.as_ref() {
                    "object" => {
                        let structsuffix = uppercase_first_letter(key);
                        Ok((format!("Vec<{}{}>", stack, structsuffix), level))
                    }
                    "string" => Ok(("Vec<String>".into(), level)),
                    "boolean" => Ok(("Vec<bool>".into(), level)),
                    "date" => Ok((format!("Vec<{}>", extract_date_type(value)?), level)),
                    "number" => Ok((format!("Vec<{}>", extract_number_type(value)?), level)),
                    "integer" => Ok((format!("Vec<{}>", extract_integer_type(value)?), level)),
                    "array" => Ok(array_recurse_for_type(s, stack, key, level + 1)?),
                    x => {
                        bail!("unsupported recursive array type {} for {}", x, key)
                    }
                };
            }
            // maybe fallback to serde_json::Value
            _ => bail!("only support single schema in array {}", key),
        }
    } else {
        bail!("missing items in array type")
    }
}

// ----------------------------------------------------------------------------
// helpers

fn extract_date_type(value: &JSONSchemaProps) -> Result<String> {
    Ok(if let Some(f) = &value.format {
        // NB: these need chrono feature on serde
        match f.as_ref() {
            // Not sure if the first actually works properly..
            // might need a Date<Utc> but chrono docs advocated for NaiveDate
            "date" => "NaiveDate".to_string(),
            "date-time" => "DateTime<Utc>".to_string(),
            x => {
                bail!("unknown date {}", x);
            }
        }
    } else {
        "String".to_string()
    })
}

fn extract_number_type(value: &JSONSchemaProps) -> Result<String> {
    // TODO: byte / password here?
    Ok(if let Some(f) = &value.format {
        match f.as_ref() {
            "float" => "f32".to_string(),
            "double" => "f64".to_string(),
            x => {
                bail!("unknown number {}", x);
            }
        }
    } else {
        "f64".to_string()
    })
}

fn extract_integer_type(value: &JSONSchemaProps) -> Result<String> {
    // Think kubernetes go types just do signed ints, but set a minimum to zero..
    // rust will set uint, so emitting that when possbile
    Ok(if let Some(f) = &value.format {
        match f.as_ref() {
            "int8" => "i8".to_string(),
            "int16" => "i16".to_string(),
            "int32" => "i32".to_string(),
            "int64" => "i64".to_string(),
            "int128" => "i128".to_string(),
            "uint8" => "u8".to_string(),
            "uint16" => "u16".to_string(),
            "uint32" => "u32".to_string(),
            "uint64" => "u64".to_string(),
            "uint128" => "u128".to_string(),
            x => {
                bail!("unknown integer {}", x);
            }
        }
    } else {
        "i64".to_string()
    })
}

fn uppercase_first_letter(s: &str) -> String {
    let mut c = s.chars();
    match c.next() {
        None => String::new(),
        Some(f) => f.to_uppercase().collect::<String>() + c.as_str(),
    }
}

// unit tests particular schema patterns
#[cfg(test)]
mod test {
    use crate::analyze;
    use k8s_openapi::apiextensions_apiserver::pkg::apis::apiextensions::v1::JSONSchemaProps;
    use serde_yaml;

    #[test]
    fn map_of_struct() {
        // validationsInfo from agent test
        let schema_str = r#"
        description: AgentStatus defines the observed state of Agent
        properties:
          validationsInfo:
            additionalProperties:
              items:
                properties:
                  id:
                    type: string
                  message:
                    type: string
                  status:
                    type: string
                required:
                - id
                - message
                - status
                type: object
              type: array
            description: ValidationsInfo is a JSON-formatted string containing
              the validation results for each validation id grouped by category
              (network, hosts-data, etc.)
            type: object
        type: object
"#;
        let schema: JSONSchemaProps = serde_yaml::from_str(schema_str).unwrap();
        //env_logger::init();
        //println!("schema: {}", serde_json::to_string_pretty(&schema).unwrap());

        let mut structs = vec![];
        analyze(schema, "ValidationsInfo", "Agent", 0, &mut structs).unwrap();
        //println!("{:?}", structs);
        let root = &structs[0];
        assert_eq!(root.name, "Agent");
        assert_eq!(root.level, 0);
        // should have a member with a key to the map:
        let map = &root.members[0];
        assert_eq!(map.name, "validationsInfo");
        assert_eq!(map.type_, "Option<BTreeMap<String, AgentValidationsInfo>>");
        // should have a separate struct
        let other = &structs[1];
        assert_eq!(other.name, "AgentValidationsInfo");
        assert_eq!(other.level, 1);
        assert_eq!(other.members[0].name, "id");
        assert_eq!(other.members[0].type_, "String");
        assert_eq!(other.members[1].name, "message");
        assert_eq!(other.members[1].type_, "String");
        assert_eq!(other.members[2].name, "status");
        assert_eq!(other.members[2].type_, "String");
    }

    #[test]
    fn empty_preserve_unknown_fields() {
        let schema_str = r#"
description: |-
  Identifies servers in the same namespace for which this authorization applies.
required:
  - selector
properties:
  selector:
    description: A label query over servers on which this authorization
      applies.
    required:
      - matchLabels
    properties:
      matchLabels:
        type: object
        x-kubernetes-preserve-unknown-fields: true
    type: object
type: object
"#;
        let schema: JSONSchemaProps = serde_yaml::from_str(schema_str).unwrap();
        //println!("schema: {}", serde_json::to_string_pretty(&schema).unwrap());
        let mut structs = vec![];
        analyze(schema, "Selector", "Server", 0, &mut structs).unwrap();
        //println!("{:#?}", structs);

        let root = &structs[0];
        assert_eq!(root.name, "Server");
        assert_eq!(root.level, 0);
        let root_member = &root.members[0];
        assert_eq!(root_member.name, "selector");
        assert_eq!(root_member.type_, "ServerSelector");
        let server_selector = &structs[1];
        assert_eq!(server_selector.name, "ServerSelector");
        assert_eq!(server_selector.level, 1);
        let match_labels = &server_selector.members[0];
        assert_eq!(match_labels.name, "matchLabels");
        assert_eq!(match_labels.type_, "BTreeMap<String, serde_json::Value>");
    }

    #[test]
    fn int_or_string() {
        let schema_str = r#"
            properties:
              port:
                description: A port name or number. Must exist in a pod spec.
                x-kubernetes-int-or-string: true
            required:
            - port
            type: object
"#;
        let schema: JSONSchemaProps = serde_yaml::from_str(schema_str).unwrap();

        let mut structs = vec![];
        analyze(schema, "ServerSpec", "Server", 0, &mut structs).unwrap();
        let root = &structs[0];
        assert_eq!(root.name, "Server");
        assert_eq!(root.level, 0);
        // should have an IntOrString member:
        let member = &root.members[0];
        assert_eq!(member.name, "port");
        assert_eq!(member.type_, "IntOrString");
        assert!(root.uses_int_or_string());
    }

    #[test]
    fn integer_handling_in_maps() {
        // via https://istio.io/latest/docs/reference/config/networking/destination-rule/
        // distribute:
        // - from: us-west/zone1/*
        //   to:
        //     "us-west/zone1/*": 80
        //     "us-west/zone2/*": 20
        // - from: us-west/zone2/*
        //   to:
        //     "us-west/zone1/*": 20
        //     "us-west/zone2/*": 80

        // i.e. distribute is an array of {from: String, to: BTreeMap<String, Integer>}
        // with the correct integer type

        // the schema is found in destinationrule-crd.yaml with this excerpt:
        let schema_str = r#"
        properties:
          distribute:
            description: 'Optional: only one of distribute, failover
              or failoverPriority can be set.'
            items:
              properties:
                from:
                  description: Originating locality, '/' separated
                  type: string
                to:
                  additionalProperties:
                    type: integer
                  description: Map of upstream localities to traffic
                    distribution weights.
                  type: object
              type: object
            type: array
        type: object
"#;
        let schema: JSONSchemaProps = serde_yaml::from_str(schema_str).unwrap();

        //println!("schema: {}", serde_json::to_string_pretty(&schema).unwrap());
        let mut structs = vec![];
        analyze(schema, "LocalityLbSetting", "DestinationRule", 1, &mut structs).unwrap();
        //println!("{:#?}", structs);

        // this should produce the root struct struct
        let root = &structs[0];
        assert_eq!(root.name, "DestinationRule");
        assert_eq!(root.level, 1);
        // which contains the distribute member:
        let distmember = &root.members[0];
        assert_eq!(distmember.name, "distribute");
        assert_eq!(distmember.type_, "Option<Vec<DestinationRuleDistribute>>");
        // which references the map type with {from,to} so find that struct:
        let ruledist = &structs[1];
        assert_eq!(ruledist.name, "DestinationRuleDistribute");
        // and has from and to members
        let from = &ruledist.members[0];
        let to = &ruledist.members[1];
        assert_eq!(from.name, "from");
        assert_eq!(to.name, "to");
        assert_eq!(from.type_, "Option<String>");
        assert_eq!(to.type_, "Option<BTreeMap<String, i64>>");
    }
}

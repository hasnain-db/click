use chrono::offset::Utc;
use chrono::{DateTime, Duration};
use clap::ArgMatches;
use humantime::parse_duration;
use k8s_openapi::{
    apimachinery::pkg::apis::meta::v1::ObjectMeta, http::Request, List, ListableResource, Metadata,
};
use prettytable::{Cell, Row};
use regex::Regex;
use serde::Deserialize;

use crate::env::Env;
use crate::error::KubeError;
use crate::kobj::KObj;
use crate::output::ClickWriter;
use crate::table::CellSpec;

use std::borrow::Cow;
use std::collections::{BTreeMap, HashMap};
use std::fmt::Debug;
use std::io::{stderr, Write};

#[macro_use]
pub mod command_def;

pub mod alias; // commands for alias/unalias
pub mod click; // commands internal to click (setting config values, etc)
pub mod configmaps; // commands relating to configmaps
pub mod delete; // command to delete objects
pub mod deployments; // command to list deployments
pub mod describe; // the describe command
pub mod events; // commands to print events
pub mod exec; // command to exec into pods
pub mod jobs; // commands relating to jobs
pub mod logs; // command to get pod logs
pub mod namespaces; // commands relating to namespaces
pub mod nodes; // commands relating to nodes
pub mod pods; //commands relating to pods
pub mod portforwards; // commands for forwarding ports
pub mod replicasets; // commands relating to relicasets
pub mod secrets; // commands for secrets
pub mod services; // commands for services
pub mod statefulsets; // commands for statefulsets
pub mod volumes; // commands relating to volumes

// utility types
type RowSpec<'a> = Vec<CellSpec<'a>>;
type Extractor<T> = fn(&T) -> Option<CellSpec<'_>>;

fn mapped_val(key: &str, map: &[(&'static str, &'static str)]) -> Option<&'static str> {
    for (map_key, val) in map.iter() {
        if &key == map_key {
            return Some(val);
        }
    }
    None
}

#[allow(clippy::too_many_arguments)] // factoring this out into structs just makes it worse
pub fn run_list_command<T, F>(
    matches: ArgMatches,
    env: &mut Env,
    writer: &mut ClickWriter,
    mut cols: Vec<&str>,
    request: Request<Vec<u8>>,
    col_map: &[(&'static str, &'static str)],
    extra_col_map: Option<&[(&'static str, &'static str)]>,
    extractors: Option<&HashMap<String, Extractor<T>>>,
    get_kobj: F,
) -> Result<(), KubeError>
where
    T: ListableResource + Metadata<Ty = ObjectMeta> + for<'de> Deserialize<'de> + Debug,
    F: Fn(&T) -> KObj,
{
    let regex = match crate::table::get_regex(&matches) {
        Ok(r) => r,
        Err(s) => {
            writeln!(stderr(), "{}", s).unwrap_or(());
            return Ok(()); // TODO: Return the error when that does something
        }
    };

    let list_opt: Option<List<T>> = env.run_on_context(|c| c.execute_list(request));

    let mut flags: Vec<&str> = match matches.values_of("show") {
        Some(v) => v.collect(),
        None => vec![],
    };

    let sort = matches
        .value_of("sort")
        .map(|s| match s.to_lowercase().as_str() {
            "age" => {
                let sf = command_def::PreExtractSort {
                    cmp: command_def::age_cmp,
                };
                command_def::SortFunc::Pre(sf)
            }
            other => {
                if let Some(col) = mapped_val(other, col_map) {
                    command_def::SortFunc::Post(col)
                } else if let Some(ecm) = extra_col_map {
                    let mut func = None;
                    for (flag, col) in ecm.iter() {
                        if flag.eq(&other) {
                            flags.push(flag);
                            func = Some(command_def::SortFunc::Post(col));
                        }
                    }
                    match func {
                        Some(f) => f,
                        None => panic!("Shouldn't be allowed to ask to sort by: {}", other),
                    }
                } else {
                    panic!("Shouldn't be allowed to ask to sort by: {}", other);
                }
            }
        });

    if let Some(ecm) = extra_col_map {
        // if we're not in a namespace, we want to add a namespace col if it's in extra_col_map
        if env.namespace.is_none() && mapped_val("namespace", ecm).is_some() {
            flags.push("namespace");
        }

        command_def::add_extra_cols(&mut cols, matches.is_present("labels"), flags, ecm);
    }

    handle_list_result(
        env,
        writer,
        cols,
        list_opt,
        extractors,
        regex,
        sort,
        matches.is_present("reverse"),
        get_kobj,
    )
}

/// Uppercase the first letter of the given str
pub fn uppercase_first(s: &str) -> String {
    let mut cs = s.chars();
    match cs.next() {
        None => String::new(),
        Some(f) => f.to_uppercase().collect::<String>() + cs.as_str(),
    }
}

/// a clap validator for duration
fn valid_duration(s: String) -> Result<(), String> {
    parse_duration(s.as_str())
        .map(|_| ())
        .map_err(|e| e.to_string())
}

/// a clap validator for rfc3339 dates
fn valid_date(s: String) -> Result<(), String> {
    DateTime::parse_from_rfc3339(s.as_str())
        .map(|_| ())
        .map_err(|e| e.to_string())
}

/// a clap validator for u32
pub fn valid_u32(s: String) -> Result<(), String> {
    s.parse::<u32>().map(|_| ()).map_err(|e| e.to_string())
}

// table printing / building
/* this function abstracts the standard handling code for when a k8s call returns a list of objects.
 * it does the following thins:
 * - builds the row specs based on the passed extractors/regex
 * - gets the kobks from each listable object
 * -- sets the env to have the built list as its current list
 * -- clears the env list if the built list was empty
 *
 * NB: This function assumes you want the printed list to be numbered. It further assumes the cols
 * will NOT include a colume named ####, and inserts it for you at the start.
 */
#[allow(clippy::too_many_arguments)]
pub fn handle_list_result<'a, T, F>(
    env: &mut Env,
    writer: &mut ClickWriter,
    cols: Vec<&str>,
    list_opt: Option<List<T>>,
    extractors: Option<&HashMap<String, Extractor<T>>>,
    regex: Option<Regex>,
    sort: Option<command_def::SortFunc<T>>,
    reverse: bool,
    get_kobj: F,
) -> Result<(), KubeError>
where
    T: 'a + ListableResource + Metadata<Ty = ObjectMeta>,
    F: Fn(&T) -> KObj,
{
    match list_opt {
        Some(mut list) => {
            if let Some(command_def::SortFunc::Pre(func)) = sort.as_ref() {
                list.items.sort_by(|a, b| (func.cmp)(a, b));
            }

            let mut specs = build_specs(&cols, &list, extractors, true, regex, get_kobj);

            let mut titles: Vec<Cell> = vec![Cell::new("####")];
            titles.reserve(cols.len());
            for col in cols.iter() {
                titles.push(Cell::new(col));
            }

            if let Some(command_def::SortFunc::Post(colname)) = sort {
                let index = cols.iter().position(|&c| c == colname);
                match index {
                    Some(index) => {
                        let idx = index + 1; // +1 for #### col
                        specs.sort_by(|a, b| a.1.get(idx).unwrap().cmp(b.1.get(idx).unwrap()));
                    }
                    None => clickwriteln!(
                        writer,
                        "Asked to sort by {}, but it's not a column in the output",
                        colname
                    ),
                }
            }

            let (kobjs, rows): (Vec<KObj>, Vec<RowSpec>) = if reverse {
                specs.into_iter().rev().unzip()
            } else {
                specs.into_iter().unzip()
            };

            crate::table::print_table_kapi(Row::new(titles), rows, writer);
            env.set_last_objs(kobjs);
        }
        None => env.clear_last_objs(),
    }
    Ok(())
}

// row building

/* Build row specs and a kobj vec from data returned from k8s.
 *
 * cols is a list of names of columns to build. "Name" * and "Age" are handled, other names need to
 * be in 'extractors', and the extractor for the specified name will be used.
 *
 * include_index = true will put an index (numbered) column as the first item in the row
 *
 * regex: if this is Some(regex) then only rows that have some cell that matches the regex will be
 * included in the output
 *
 * get_kobj: this needs to be a function that maps the list items to crate::kobj::KObjs
 *
 * This returns the vector of built kobjs that can be then passed to the env to set the last list of
 * things returned, and the row specs that can be used to print out that list.
 */
pub fn build_specs<'a, T, F>(
    cols: &[&str],
    list: &'a List<T>,
    extractors: Option<&HashMap<String, Extractor<T>>>,
    include_index: bool,
    regex: Option<Regex>,
    get_kobj: F,
) -> Vec<(KObj, RowSpec<'a>)>
where
    T: 'a + ListableResource + Metadata<Ty = ObjectMeta>,
    F: Fn(&T) -> KObj,
{
    let mut ret = vec![];
    for item in list.items.iter() {
        let mut row: Vec<CellSpec> = if include_index {
            vec![CellSpec::new_index()]
        } else {
            vec![]
        };
        for col in cols.iter() {
            match *col {
                "Age" => row.push(extract_age(item).into()),
                "Labels" => row.push(extract_labels(item).into()),
                "Name" => row.push(extract_name(item).into()),
                "Namespace" => row.push(extract_namespace(item).into()),
                _ => match extractors {
                    Some(extractors) => match extractors.get(*col) {
                        Some(extractor) => row.push(extractor(item).into()),
                        None => panic!("Can't extract"),
                    },
                    None => panic!("Can't extract"),
                },
            }
        }
        match regex {
            Some(ref regex) => {
                if row_matches(&row, regex) {
                    ret.push((get_kobj(item), row));
                }
            }
            None => {
                ret.push((get_kobj(item), row));
            }
        }
    }
    ret
}

// common extractors

/// An extractor for the Name field. Extracts the name out of the object metadata
pub fn extract_name<T: Metadata<Ty = ObjectMeta>>(obj: &T) -> Option<Cow<'_, str>> {
    let meta = obj.metadata();
    meta.name.as_ref().map(|n| n.into())
}

/// An extractor for the Age field. Extracts the age out of the object metadata
pub fn extract_age<T: Metadata<Ty = ObjectMeta>>(obj: &T) -> Option<Cow<'_, str>> {
    let meta = obj.metadata();
    meta.creation_timestamp
        .as_ref()
        .map(|ts| time_since(ts.0).into())
}

/// An extractor for the Namespace field. Extracts the namespace out of the object metadata
pub fn extract_namespace<T: Metadata<Ty = ObjectMeta>>(obj: &T) -> Option<Cow<'_, str>> {
    let meta = obj.metadata();
    meta.namespace.as_ref().map(|ns| ns.as_str().into())
}

/// An extractor for the Labels field. Extracts the labels out of the object metadata
pub fn extract_labels<T: Metadata<Ty = ObjectMeta>>(obj: &T) -> Option<Cow<'_, str>> {
    let meta = obj.metadata();
    Some(keyval_string(&meta.labels).into())
}

// utility functions
fn row_matches<'a>(row: &[CellSpec<'a>], regex: &Regex) -> bool {
    let mut has_match = false;
    for cell_spec in row.iter() {
        if !has_match {
            has_match = cell_spec.matches(regex);
        }
    }
    has_match
}

pub fn format_duration(duration: Duration) -> String {
    if duration.num_days() > 365 {
        // TODO: maybe be more smart about printing years, or at least have an option
        let days = duration.num_days();
        let yrs = days / 365;
        format!("{}y {}d", yrs, (duration.num_days() - (yrs * 365)))
    } else if duration.num_days() > 0 {
        format!(
            "{}d {}h",
            duration.num_days(),
            (duration.num_hours() - (24 * duration.num_days()))
        )
    } else if duration.num_hours() > 0 {
        format!(
            "{}h {}m",
            duration.num_hours(),
            (duration.num_minutes() - (60 * duration.num_hours()))
        )
    } else if duration.num_minutes() > 0 {
        format!(
            "{}m {}s",
            duration.num_minutes(),
            (duration.num_seconds() - (60 * duration.num_minutes()))
        )
    } else {
        format!("{}s", duration.num_seconds())
    }
}

pub fn time_since(date: DateTime<Utc>) -> String {
    let now = Utc::now();
    let diff = now.signed_duration_since(date);
    format_duration(diff)
}

/// Build a multi-line string of the specified keyvals
pub fn keyval_string(keyvals: &BTreeMap<String, String>) -> String {
    let mut buf = String::new();
    for (key, val) in keyvals.iter() {
        buf.push_str(key);
        buf.push('=');
        buf.push_str(val);
        buf.push('\n');
    }
    buf
}

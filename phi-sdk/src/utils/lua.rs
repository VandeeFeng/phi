use std::sync::{Arc, Mutex};

use mlua::{Lua, LuaOptions, StdLib, Table, Value};
use serde_json::Map;

pub(crate) fn isolated_lua() -> mlua::Result<Lua> {
    let lua = Lua::new_with(
        StdLib::COROUTINE | StdLib::TABLE | StdLib::STRING | StdLib::UTF8 | StdLib::MATH,
        LuaOptions::default(),
    )?;
    let globals = lua.globals();
    globals.set("dofile", Value::Nil)?;
    globals.set("loadfile", Value::Nil)?;
    let tostring: mlua::Function = globals.get("tostring")?;
    globals.set(
        "print",
        lua.create_function(move |_, values: mlua::Variadic<Value>| {
            let message = values
                .into_iter()
                .map(|value| {
                    tostring
                        .call::<mlua::LuaString>(value)
                        .map(|value| value.to_string_lossy())
                })
                .collect::<mlua::Result<Vec<_>>>()?
                .join("\t");
            tracing::info!(source = "lua.print", "{message}");
            Ok(())
        })?,
    )?;
    drop(globals);
    Ok(lua)
}

pub(crate) fn json_to_lua(lua: &Lua, value: &serde_json::Value) -> mlua::Result<Value> {
    Ok(match value {
        serde_json::Value::Null => Value::Nil,
        serde_json::Value::Bool(value) => Value::Boolean(*value),
        serde_json::Value::Number(value) => {
            if let Some(value) = value.as_i64() {
                Value::Integer(value)
            } else {
                Value::Number(value.as_f64().unwrap_or_default())
            }
        }
        serde_json::Value::String(value) => Value::String(lua.create_string(value)?),
        serde_json::Value::Array(items) => json_array_to_lua(lua, items)?,
        serde_json::Value::Object(map) => json_object_to_lua(lua, map)?,
    })
}

fn json_array_to_lua(lua: &Lua, items: &[serde_json::Value]) -> mlua::Result<Value> {
    let table = lua.create_table()?;
    for (index, item) in items.iter().enumerate() {
        table.raw_set(index + 1, json_to_lua(lua, item)?)?;
    }
    Ok(Value::Table(table))
}

fn json_object_to_lua(lua: &Lua, map: &Map<String, serde_json::Value>) -> mlua::Result<Value> {
    let table = lua.create_table()?;
    for (key, value) in map {
        table.set(key.as_str(), json_to_lua(lua, value)?)?;
    }
    Ok(Value::Table(table))
}

pub(crate) fn lua_table_to_json(table: &Table) -> mlua::Result<serde_json::Value> {
    if table.raw_len() > 0 && lua_table_is_array(table)? {
        return lua_array_to_json(table);
    }

    let mut map = serde_json::Map::new();
    for pair in table.clone().pairs::<Value, Value>() {
        let (key, value) = pair?;
        let Value::String(key) = key else {
            continue;
        };
        map.insert(key.to_str()?.to_string(), lua_to_json(value)?);
    }
    Ok(serde_json::Value::Object(map))
}

fn lua_table_is_array(table: &Table) -> mlua::Result<bool> {
    let len = table.raw_len();
    let mut count = 0;
    for pair in table.clone().pairs::<Value, Value>() {
        let (key, _) = pair?;
        let Value::Integer(index) = key else {
            return Ok(false);
        };
        if index < 1 || index as usize > len {
            return Ok(false);
        }
        count += 1;
    }
    Ok(count == len)
}

fn lua_array_to_json(table: &Table) -> mlua::Result<serde_json::Value> {
    let mut items = Vec::with_capacity(table.raw_len());
    for index in 1..=table.raw_len() {
        items.push(lua_to_json(table.raw_get(index)?)?);
    }
    Ok(serde_json::Value::Array(items))
}

pub(crate) fn lua_to_json(value: Value) -> mlua::Result<serde_json::Value> {
    Ok(match value {
        Value::Nil => serde_json::Value::Null,
        Value::Boolean(value) => serde_json::Value::Bool(value),
        Value::Integer(value) => serde_json::Value::Number(value.into()),
        Value::Number(value) => serde_json::Number::from_f64(value)
            .map(serde_json::Value::Number)
            .unwrap_or(serde_json::Value::Null),
        Value::String(value) => serde_json::Value::String(value.to_str()?.to_string()),
        Value::Table(table) => lua_table_to_json(&table)?,
        _ => serde_json::Value::Null,
    })
}

pub(crate) fn create_context(
    lua: &Lua,
    notifications: Arc<Mutex<Vec<String>>>,
) -> mlua::Result<Table> {
    let ctx = lua.create_table()?;
    let ui = lua.create_table()?;
    ui.set(
        "notify",
        lua.create_function(move |_, text: String| {
            if !text.trim().is_empty() {
                notifications
                    .lock()
                    .map_err(|_| mlua::Error::runtime("notification lock poisoned"))?
                    .push(text);
            }
            Ok(())
        })?,
    )?;
    ctx.set("ui", ui)?;
    Ok(ctx)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lua_print_uses_diagnostic_logging() -> mlua::Result<()> {
        let path = std::env::temp_dir().join(format!("phi-lua-log-{}", uuid::Uuid::new_v4()));
        let output = std::fs::File::create(&path).unwrap();
        let subscriber = tracing_subscriber::fmt()
            .without_time()
            .with_ansi(false)
            .with_writer(Mutex::new(output))
            .finish();
        tracing::subscriber::with_default(subscriber, || -> mlua::Result<()> {
            isolated_lua()?
                .load(
                    r#"
                print("extension diagnostic", 42, true, nil)
                print(setmetatable({}, { __tostring = function() return "custom value" end }))
            "#,
                )
                .exec()
        })?;
        let output = std::fs::read_to_string(&path).unwrap();
        std::fs::remove_file(path).unwrap();
        assert!(output.contains("extension diagnostic\t42\ttrue\tnil"));
        assert!(output.contains("custom value"));
        Ok(())
    }

    #[test]
    fn lua_table_to_json_preserves_arrays() -> mlua::Result<()> {
        let lua = Lua::new();
        let table: Table = lua.load(r#"{ values = { "one", "two" } }"#).eval()?;

        let value = lua_table_to_json(&table)?;

        assert_eq!(value["values"], serde_json::json!(["one", "two"]));
        Ok(())
    }
}

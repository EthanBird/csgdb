//! Derive support for stable, explicitly named CSGDB collections.

use proc_macro::TokenStream;
use proc_macro2::{Ident, TokenStream as TokenStream2};
use quote::{format_ident, quote};
use sha2::{Digest, Sha256};
use std::collections::HashSet;
use std::fmt::Write as _;
use syn::{
    parse_macro_input, Data, DeriveInput, Fields, GenericArgument, LitInt, LitStr, PathArguments,
    Type,
};

#[proc_macro_derive(Collection, attributes(csgdb))]
/// Derives stable schema metadata and typed CRUD codecs for a named struct.
///
/// The struct-level `csgdb` attribute requires `collection`, `table`, and
/// `version`. Every field requires `id` and `column`; exactly one field must
/// also specify `primary_key`. An optional struct-level `crate` string selects
/// a renamed CSGDB dependency path.
pub fn derive_collection(input: TokenStream) -> TokenStream {
    let input = parse_macro_input!(input as DeriveInput);
    expand_collection(&input)
        .unwrap_or_else(syn::Error::into_compile_error)
        .into()
}

#[derive(Default)]
struct CollectionAttributes {
    id: Option<LitStr>,
    table: Option<LitStr>,
    version: Option<LitInt>,
    crate_path: Option<LitStr>,
    indexes: Vec<IndexAttributes>,
}

#[derive(Default)]
struct IndexAttributes {
    id: Option<LitStr>,
    name: Option<LitStr>,
    fields: Vec<LitStr>,
    unique: bool,
}

#[derive(Default)]
struct FieldAttributes {
    id: Option<LitStr>,
    column: Option<LitStr>,
    primary_key: bool,
}

struct DerivedField<'input> {
    rust_name: &'input Ident,
    ty: &'input Type,
    id: String,
    column: String,
    storage: &'static str,
    nullable: bool,
    primary_key: bool,
}

struct DerivedIndex {
    id: String,
    name: String,
    columns: Vec<String>,
    unique: bool,
    canonical: String,
    fingerprint: [u8; 32],
}

#[allow(clippy::too_many_lines)]
fn expand_collection(input: &DeriveInput) -> syn::Result<TokenStream2> {
    if !input.generics.params.is_empty() {
        return Err(syn::Error::new_spanned(
            &input.generics,
            "Collection does not support generic structs",
        ));
    }

    let attributes = parse_collection_attributes(input)?;
    let collection = required_string(attributes.id, input, "collection")?;
    let table = required_string(attributes.table, input, "table")?;
    let version = attributes
        .version
        .ok_or_else(|| syn::Error::new_spanned(input, "missing csgdb version"))?
        .base10_parse::<u32>()?;
    validate_stable_name(&collection, input, "collection")?;
    validate_sql_name(&table, input, "table")?;
    if version == 0 {
        return Err(syn::Error::new_spanned(
            input,
            "csgdb version must be greater than zero",
        ));
    }

    let named_fields = match &input.data {
        Data::Struct(data) => match &data.fields {
            Fields::Named(fields) => &fields.named,
            _ => {
                return Err(syn::Error::new_spanned(
                    input,
                    "Collection requires a struct with named fields",
                ));
            }
        },
        _ => {
            return Err(syn::Error::new_spanned(
                input,
                "Collection can only be derived for structs",
            ));
        }
    };
    if named_fields.is_empty() {
        return Err(syn::Error::new_spanned(
            input,
            "Collection requires at least one field",
        ));
    }

    let mut derived_fields = Vec::with_capacity(named_fields.len());
    let mut field_ids = HashSet::with_capacity(named_fields.len());
    let mut column_names = HashSet::with_capacity(named_fields.len());
    for field in named_fields {
        let rust_name = field.ident.as_ref().expect("named field");
        let field_attributes = parse_field_attributes(field)?;
        let id = required_string(field_attributes.id, field, "id")?;
        let column = required_string(field_attributes.column, field, "column")?;
        validate_stable_name(&id, field, "field id")?;
        validate_sql_name(&column, field, "column")?;
        if !field_ids.insert(id.clone()) {
            return Err(syn::Error::new_spanned(field, "duplicate csgdb field id"));
        }
        if !column_names.insert(column.clone()) {
            return Err(syn::Error::new_spanned(
                field,
                "duplicate csgdb column name",
            ));
        }
        let (storage, nullable) = classify_type(&field.ty)?;
        if field_attributes.primary_key && nullable {
            return Err(syn::Error::new_spanned(
                field,
                "the primary key cannot be optional",
            ));
        }
        derived_fields.push(DerivedField {
            rust_name,
            ty: &field.ty,
            id,
            column,
            storage,
            nullable,
            primary_key: field_attributes.primary_key,
        });
    }

    let primary_keys = derived_fields
        .iter()
        .filter(|field| field.primary_key)
        .collect::<Vec<_>>();
    if primary_keys.len() != 1 {
        return Err(syn::Error::new_spanned(
            input,
            "Collection requires exactly one #[csgdb(primary_key)] field",
        ));
    }
    let primary_key = primary_keys[0];

    let indexes = derive_indexes(
        &collection,
        &table,
        attributes.indexes,
        &derived_fields,
        input,
    )?;

    let canonical = canonical_schema(&collection, &table, version, &derived_fields);
    let fingerprint = Sha256::digest(canonical.as_bytes());
    let fingerprint_bytes = fingerprint.iter();
    let quoted_table = quote_identifier(&table);
    let create_sql = create_table_sql(&quoted_table, &derived_fields);
    let insert_sql = insert_sql(&quoted_table, &derived_fields);
    let select_sql = select_sql(&quoted_table, &derived_fields, &primary_key.column);
    let update_sql = update_sql(&quoted_table, &derived_fields, &primary_key.column);
    let delete_sql = delete_sql(&quoted_table, &primary_key.column);
    let create_index_sql = indexes
        .iter()
        .map(|index| create_index_sql(&quoted_table, index))
        .collect::<Vec<_>>();

    let crate_path = if let Some(literal) = attributes.crate_path {
        let path = syn::parse_str::<syn::Path>(&literal.value()).map_err(|_| {
            syn::Error::new_spanned(&literal, "csgdb crate must be a valid Rust path")
        })?;
        quote!(#path)
    } else {
        quote!(::csgdb)
    };
    let name = &input.ident;
    let field_count = derived_fields.len();
    let index_count = indexes.len();
    let field_schemas = derived_fields.iter().map(|field| {
        let id = &field.id;
        let column = &field.column;
        let nullable = field.nullable;
        let primary_key = field.primary_key;
        let column_type = format_ident!("{}", field.storage);
        quote! {
            #crate_path::FieldSchema::new(
                #id,
                #column,
                #crate_path::ColumnType::#column_type,
                #nullable,
                #primary_key,
            )
        }
    });
    let value_encoders = derived_fields.iter().map(|field| {
        let field_name = field.rust_name;
        let ty = field.ty;
        quote! { <#ty as #crate_path::FieldValue>::to_value(&self.#field_name)? }
    });
    let value_decoders = derived_fields.iter().enumerate().map(|(index, field)| {
        let field_name = field.rust_name;
        let ty = field.ty;
        quote! {
            #field_name: <#ty as #crate_path::FieldValue>::from_value(row.value_ref(#index)?)?
        }
    });
    let primary_key_name = primary_key.rust_name;
    let primary_key_type = primary_key.ty;
    let index_schemas = indexes.iter().map(|index| {
        let id = &index.id;
        let name = &index.name;
        let columns = &index.columns;
        let unique = index.unique;
        let canonical = &index.canonical;
        let fingerprint = index.fingerprint.iter();
        quote! {
            #crate_path::IndexSchema::new(
                #id,
                #name,
                &[#(#columns),*],
                #unique,
                #crate_path::SchemaFingerprint::new([#(#fingerprint),*]),
                #canonical,
            )
        }
    });

    Ok(quote! {
        #[automatically_derived]
        impl #crate_path::Collection for #name {
            type Key = #primary_key_type;

            const CREATE_TABLE_SQL: &'static str = #create_sql;
            const INSERT_SQL: &'static str = #insert_sql;
            const SELECT_BY_KEY_SQL: &'static str = #select_sql;
            const UPDATE_SQL: &'static str = #update_sql;
            const DELETE_BY_KEY_SQL: &'static str = #delete_sql;
            const CREATE_INDEX_SQL: &'static [&'static str] = &[#(#create_index_sql),*];

            fn schema() -> &'static #crate_path::CollectionSchema {
                static FIELDS: [#crate_path::FieldSchema; #field_count] = [
                    #(#field_schemas),*
                ];
                static INDEXES: [#crate_path::IndexSchema; #index_count] = [
                    #(#index_schemas),*
                ];
                static SCHEMA: #crate_path::CollectionSchema = #crate_path::CollectionSchema::new(
                    #collection,
                    #table,
                    #version,
                    #crate_path::SchemaFingerprint::new([#(#fingerprint_bytes),*]),
                    &FIELDS,
                    #canonical,
                    &INDEXES,
                );
                &SCHEMA
            }

            fn values(&self) -> #crate_path::Result<Vec<#crate_path::Value>> {
                Ok(vec![#(#value_encoders),*])
            }

            fn key_value(&self) -> #crate_path::Result<#crate_path::Value> {
                <#primary_key_type as #crate_path::FieldValue>::to_value(&self.#primary_key_name)
            }

            fn from_row(row: &#crate_path::Row<'_>) -> #crate_path::Result<Self> {
                Ok(Self {
                    #(#value_decoders),*
                })
            }
        }
    })
}

fn parse_collection_attributes(input: &DeriveInput) -> syn::Result<CollectionAttributes> {
    let mut result = CollectionAttributes::default();
    for attribute in &input.attrs {
        if !attribute.path().is_ident("csgdb") {
            continue;
        }
        attribute.parse_nested_meta(|meta| {
            if meta.path.is_ident("collection") {
                set_once(&mut result.id, meta.value()?.parse()?, &meta.path)
            } else if meta.path.is_ident("table") {
                set_once(&mut result.table, meta.value()?.parse()?, &meta.path)
            } else if meta.path.is_ident("version") {
                if result.version.is_some() {
                    return Err(meta.error("duplicate csgdb version"));
                }
                result.version = Some(meta.value()?.parse()?);
                Ok(())
            } else if meta.path.is_ident("crate") {
                set_once(&mut result.crate_path, meta.value()?.parse()?, &meta.path)
            } else if meta.path.is_ident("index") {
                let mut index = IndexAttributes::default();
                meta.parse_nested_meta(|nested| {
                    if nested.path.is_ident("id") {
                        set_once(&mut index.id, nested.value()?.parse()?, &nested.path)
                    } else if nested.path.is_ident("name") {
                        set_once(&mut index.name, nested.value()?.parse()?, &nested.path)
                    } else if nested.path.is_ident("field") {
                        index.fields.push(nested.value()?.parse()?);
                        Ok(())
                    } else if nested.path.is_ident("unique") {
                        if index.unique {
                            return Err(nested.error("duplicate csgdb index unique"));
                        }
                        index.unique = true;
                        Ok(())
                    } else {
                        Err(nested.error("unsupported csgdb index attribute"))
                    }
                })?;
                result.indexes.push(index);
                Ok(())
            } else {
                Err(meta.error("unsupported csgdb collection attribute"))
            }
        })?;
    }
    Ok(result)
}

fn derive_indexes(
    collection: &str,
    table: &str,
    attributes: Vec<IndexAttributes>,
    fields: &[DerivedField<'_>],
    input: &DeriveInput,
) -> syn::Result<Vec<DerivedIndex>> {
    let mut result = Vec::with_capacity(attributes.len());
    let mut ids = HashSet::with_capacity(attributes.len());
    let mut names = HashSet::with_capacity(attributes.len());
    for attributes in attributes {
        let id = required_string(attributes.id, input, "index id")?;
        let name = required_string(attributes.name, input, "index name")?;
        validate_stable_name(&id, input, "index id")?;
        validate_sql_name(&name, input, "index name")?;
        if name.starts_with("__csgdb_") {
            return Err(syn::Error::new_spanned(
                input,
                "index names beginning with __csgdb_ are reserved",
            ));
        }
        if !ids.insert(id.clone()) {
            return Err(syn::Error::new_spanned(input, "duplicate csgdb index id"));
        }
        if !names.insert(name.clone()) {
            return Err(syn::Error::new_spanned(input, "duplicate csgdb index name"));
        }
        if attributes.fields.is_empty() {
            return Err(syn::Error::new_spanned(
                input,
                "csgdb index requires at least one field",
            ));
        }

        let mut field_ids = Vec::with_capacity(attributes.fields.len());
        let mut columns = Vec::with_capacity(attributes.fields.len());
        let mut seen_fields = HashSet::with_capacity(attributes.fields.len());
        for literal in attributes.fields {
            let field_id = literal.value();
            if !seen_fields.insert(field_id.clone()) {
                return Err(syn::Error::new_spanned(
                    literal,
                    "duplicate field in csgdb index",
                ));
            }
            let field = fields
                .iter()
                .find(|field| field.id == field_id)
                .ok_or_else(|| {
                    syn::Error::new_spanned(literal, "csgdb index references an unknown field id")
                })?;
            field_ids.push(field_id);
            columns.push(field.column.clone());
        }
        let canonical = canonical_index(
            collection,
            table,
            &id,
            &name,
            attributes.unique,
            &field_ids,
            &columns,
        );
        let fingerprint = Sha256::digest(canonical.as_bytes()).into();
        result.push(DerivedIndex {
            id,
            name,
            columns,
            unique: attributes.unique,
            canonical,
            fingerprint,
        });
    }
    Ok(result)
}

fn parse_field_attributes(field: &syn::Field) -> syn::Result<FieldAttributes> {
    let mut result = FieldAttributes::default();
    for attribute in &field.attrs {
        if !attribute.path().is_ident("csgdb") {
            continue;
        }
        attribute.parse_nested_meta(|meta| {
            if meta.path.is_ident("id") {
                set_once(&mut result.id, meta.value()?.parse()?, &meta.path)
            } else if meta.path.is_ident("column") {
                set_once(&mut result.column, meta.value()?.parse()?, &meta.path)
            } else if meta.path.is_ident("primary_key") {
                if result.primary_key {
                    return Err(meta.error("duplicate csgdb primary_key"));
                }
                result.primary_key = true;
                Ok(())
            } else {
                Err(meta.error("unsupported csgdb field attribute"))
            }
        })?;
    }
    Ok(result)
}

fn set_once(destination: &mut Option<LitStr>, value: LitStr, path: &syn::Path) -> syn::Result<()> {
    if destination.replace(value).is_some() {
        Err(syn::Error::new_spanned(path, "duplicate csgdb attribute"))
    } else {
        Ok(())
    }
}

fn required_string<T: quote::ToTokens>(
    value: Option<LitStr>,
    span: T,
    name: &str,
) -> syn::Result<String> {
    value
        .map(|literal| literal.value())
        .ok_or_else(|| syn::Error::new_spanned(span, format!("missing csgdb {name}")))
}

fn validate_stable_name<T: quote::ToTokens>(value: &str, span: T, kind: &str) -> syn::Result<()> {
    if value.is_empty() || value.len() > 255 || value.contains('\0') {
        Err(syn::Error::new_spanned(
            span,
            format!("csgdb {kind} must contain 1 to 255 non-NUL bytes"),
        ))
    } else {
        Ok(())
    }
}

fn validate_sql_name<T: quote::ToTokens>(value: &str, span: T, kind: &str) -> syn::Result<()> {
    validate_stable_name(value, &span, kind)?;
    if kind == "table" && value.starts_with("__csgdb_") {
        Err(syn::Error::new_spanned(
            span,
            "table names beginning with __csgdb_ are reserved",
        ))
    } else {
        Ok(())
    }
}

fn classify_type(ty: &Type) -> syn::Result<(&'static str, bool)> {
    if let Some(inner) = generic_inner(ty, "Option") {
        let (storage, nested_nullable) = classify_type(inner)?;
        if nested_nullable {
            return Err(syn::Error::new_spanned(
                ty,
                "nested Option fields are not supported",
            ));
        }
        return Ok((storage, true));
    }

    if is_byte_vector(ty) {
        return Ok(("Blob", false));
    }
    let Some(ident) = simple_type_ident(ty) else {
        return Err(unsupported_field_type(ty));
    };
    let storage = match ident.to_string().as_str() {
        "i8" | "i16" | "i32" | "i64" | "u8" | "u16" | "u32" | "u64" | "bool" => "Integer",
        "f32" | "f64" => "Real",
        "String" => "Text",
        _ => return Err(unsupported_field_type(ty)),
    };
    Ok((storage, false))
}

fn generic_inner<'type_>(ty: &'type_ Type, expected: &str) -> Option<&'type_ Type> {
    let Type::Path(path) = ty else {
        return None;
    };
    let segment = path.path.segments.last()?;
    if segment.ident != expected {
        return None;
    }
    let PathArguments::AngleBracketed(arguments) = &segment.arguments else {
        return None;
    };
    if arguments.args.len() != 1 {
        return None;
    }
    match arguments.args.first()? {
        GenericArgument::Type(inner) => Some(inner),
        _ => None,
    }
}

fn is_byte_vector(ty: &Type) -> bool {
    generic_inner(ty, "Vec")
        .and_then(simple_type_ident)
        .is_some_and(|ident| ident == "u8")
}

fn simple_type_ident(ty: &Type) -> Option<&Ident> {
    let Type::Path(path) = ty else {
        return None;
    };
    if path.qself.is_some() {
        return None;
    }
    path.path.segments.last().map(|segment| &segment.ident)
}

fn unsupported_field_type(ty: &Type) -> syn::Error {
    syn::Error::new_spanned(
        ty,
        "unsupported Collection field type; use integers, floats, bool, String, Vec<u8>, or Option<T>",
    )
}

fn canonical_schema(
    collection: &str,
    table: &str,
    version: u32,
    fields: &[DerivedField<'_>],
) -> String {
    let mut sorted = fields.iter().collect::<Vec<_>>();
    sorted.sort_unstable_by(|left, right| left.id.cmp(&right.id));
    let mut canonical = format!(
        "csgdb-schema-v1\ncollection:{}:{collection}\ntable:{}:{table}\nversion:{version}\n",
        collection.len(),
        table.len(),
    );
    for field in sorted {
        writeln!(
            canonical,
            "field:{}:{}:{}:{}:{}:{}:{}",
            field.id.len(),
            field.id,
            field.column.len(),
            field.column,
            field.storage,
            u8::from(field.nullable),
            u8::from(field.primary_key),
        )
        .expect("writing to a String cannot fail");
    }
    canonical
}

fn canonical_index(
    collection: &str,
    table: &str,
    id: &str,
    name: &str,
    unique: bool,
    field_ids: &[String],
    columns: &[String],
) -> String {
    let mut canonical = format!(
        "csgdb-index-v1\ncollection:{}:{collection}\ntable:{}:{table}\nindex:{}:{id}\nname:{}:{name}\nunique:{}\n",
        collection.len(),
        table.len(),
        id.len(),
        name.len(),
        u8::from(unique),
    );
    for (field, column) in field_ids.iter().zip(columns) {
        writeln!(
            canonical,
            "field:{}:{}:{}:{}",
            field.len(),
            field,
            column.len(),
            column,
        )
        .expect("writing to a String cannot fail");
    }
    canonical
}

fn create_table_sql(table: &str, fields: &[DerivedField<'_>]) -> String {
    let columns = fields
        .iter()
        .map(|field| {
            let column = quote_identifier(&field.column);
            let mut definition = format!("{column} {}", field.storage.to_ascii_uppercase());
            if !field.nullable {
                definition.push_str(" NOT NULL");
            }
            if field.primary_key {
                definition.push_str(" PRIMARY KEY");
            }
            definition
        })
        .collect::<Vec<_>>()
        .join(", ");
    format!("CREATE TABLE IF NOT EXISTS {table} ({columns})")
}

fn create_index_sql(table: &str, index: &DerivedIndex) -> String {
    let unique = if index.unique { "UNIQUE " } else { "" };
    let columns = index
        .columns
        .iter()
        .map(|column| quote_identifier(column))
        .collect::<Vec<_>>()
        .join(", ");
    format!(
        "CREATE {unique}INDEX IF NOT EXISTS {} ON {table} ({columns})",
        quote_identifier(&index.name),
    )
}

fn insert_sql(table: &str, fields: &[DerivedField<'_>]) -> String {
    let columns = quoted_columns(fields);
    let parameters = (1..=fields.len())
        .map(|index| format!("?{index}"))
        .collect::<Vec<_>>()
        .join(", ");
    format!("INSERT INTO {table} ({columns}) VALUES ({parameters})")
}

fn select_sql(table: &str, fields: &[DerivedField<'_>], primary_key: &str) -> String {
    let columns = quoted_columns(fields);
    let primary_key = quote_identifier(primary_key);
    format!("SELECT {columns} FROM {table} WHERE {primary_key} = ?1 LIMIT 1")
}

fn update_sql(table: &str, fields: &[DerivedField<'_>], primary_key: &str) -> String {
    let mutable = fields
        .iter()
        .filter(|field| !field.primary_key)
        .enumerate()
        .map(|(index, field)| format!("{} = ?{}", quote_identifier(&field.column), index + 1))
        .collect::<Vec<_>>();
    let primary_key = quote_identifier(primary_key);
    if mutable.is_empty() {
        format!("UPDATE {table} SET {primary_key} = {primary_key} WHERE {primary_key} = ?1")
    } else {
        let key_parameter = mutable.len() + 1;
        format!(
            "UPDATE {table} SET {} WHERE {primary_key} = ?{key_parameter}",
            mutable.join(", ")
        )
    }
}

fn delete_sql(table: &str, primary_key: &str) -> String {
    let primary_key = quote_identifier(primary_key);
    format!("DELETE FROM {table} WHERE {primary_key} = ?1")
}

fn quoted_columns(fields: &[DerivedField<'_>]) -> String {
    fields
        .iter()
        .map(|field| quote_identifier(&field.column))
        .collect::<Vec<_>>()
        .join(", ")
}

fn quote_identifier(value: &str) -> String {
    format!("\"{}\"", value.replace('"', "\"\""))
}

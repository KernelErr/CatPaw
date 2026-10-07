//! `URL` and `URLSearchParams` (<https://url.spec.whatwg.org/#api>).

use catpaw_js::{Exception, Fallible, ObjectId};
use url::Url;
use url::form_urlencoded;

use crate::generated::{
    self as web, StringSequenceSequenceOrStringStringRecordOrString as ParamsInit,
};
use crate::page::Cx;
use crate::{Web, platform_object};

pub struct UrlObject {
    url: Url,
    /// The `searchParams` object, once script has asked for it.
    search_params: Option<ObjectId>,
}
platform_object!(UrlObject, URL);

pub struct SearchParamsObject {
    list: Vec<(String, String)>,
    /// The URL whose query this object edits, if it came from `url.searchParams`.
    url: Option<ObjectId>,
}
platform_object!(SearchParamsObject, URLSearchParams);

fn parse_query(query: &str) -> Vec<(String, String)> {
    form_urlencoded::parse(query.as_bytes())
        .into_owned()
        .collect()
}

fn serialize_query(list: &[(String, String)]) -> String {
    form_urlencoded::Serializer::new(String::new())
        .extend_pairs(list)
        .finish()
}

fn parse_url(input: &str, base: Option<&str>) -> Option<Url> {
    match base {
        Some(base) => Url::parse(base).ok()?.join(input).ok(),
        None => Url::parse(input).ok(),
    }
}

fn invalid_url(input: &str) -> Exception {
    Exception::type_error(format!("'{input}' is not a valid URL"))
}

fn with_url<R>(cx: &Cx<'_>, this: ObjectId, f: impl FnOnce(&mut UrlObject) -> R) -> Fallible<R> {
    cx.page.with::<UrlObject, _>(this, f)
}

/// Copies the URL's query into its `searchParams` object after the URL
/// changed.
fn sync_params_from_url(cx: &Cx<'_>, this: ObjectId) -> Fallible<()> {
    let (params, query) = with_url(cx, this, |u| {
        (
            u.search_params,
            u.url.query().unwrap_or_default().to_string(),
        )
    })?;
    if let Some(params) = params {
        let _ = cx
            .page
            .with::<SearchParamsObject, _>(params, |p| p.list = parse_query(&query));
    }
    Ok(())
}

/// Edits the URL with one of the `url::quirks` setters.
fn edit(cx: &Cx<'_>, this: ObjectId, f: impl FnOnce(&mut Url)) -> Fallible<()> {
    with_url(cx, this, |u| f(&mut u.url))?;
    sync_params_from_url(cx, this)
}

fn read(cx: &Cx<'_>, this: ObjectId, f: impl FnOnce(&Url) -> &str) -> Fallible<String> {
    with_url(cx, this, |u| f(&u.url).to_string())
}

impl web::URLImpl for Web {
    fn create_object_url(cx: &mut Cx<'_>, obj: ObjectId) -> Fallible<String> {
        crate::file_api::create_object_url(cx, obj)
    }

    fn revoke_object_url(cx: &mut Cx<'_>, url: String) -> Fallible<()> {
        crate::file_api::revoke_object_url(cx.page, &url);
        Ok(())
    }

    fn parse(cx: &mut Cx<'_>, url: String, base: Option<String>) -> Fallible<Option<ObjectId>> {
        Ok(parse_url(&url, base.as_deref()).map(|url| {
            cx.page.alloc(UrlObject {
                url,
                search_params: None,
            })
        }))
    }

    fn can_parse(_cx: &mut Cx<'_>, url: String, base: Option<String>) -> Fallible<bool> {
        Ok(parse_url(&url, base.as_deref()).is_some())
    }

    fn href(cx: &mut Cx<'_>, this: ObjectId) -> Fallible<String> {
        read(cx, this, url::quirks::href)
    }

    fn set_href(cx: &mut Cx<'_>, this: ObjectId, value: String) -> Fallible<()> {
        let parsed = Url::parse(&value).map_err(|_| invalid_url(&value))?;
        edit(cx, this, |u| *u = parsed)
    }

    fn origin(cx: &mut Cx<'_>, this: ObjectId) -> Fallible<String> {
        with_url(cx, this, |u| url::quirks::origin(&u.url))
    }

    fn protocol(cx: &mut Cx<'_>, this: ObjectId) -> Fallible<String> {
        read(cx, this, url::quirks::protocol)
    }

    fn set_protocol(cx: &mut Cx<'_>, this: ObjectId, value: String) -> Fallible<()> {
        edit(cx, this, |u| {
            let _ = url::quirks::set_protocol(u, &value);
        })
    }

    fn username(cx: &mut Cx<'_>, this: ObjectId) -> Fallible<String> {
        read(cx, this, url::quirks::username)
    }

    fn set_username(cx: &mut Cx<'_>, this: ObjectId, value: String) -> Fallible<()> {
        edit(cx, this, |u| {
            let _ = url::quirks::set_username(u, &value);
        })
    }

    fn password(cx: &mut Cx<'_>, this: ObjectId) -> Fallible<String> {
        read(cx, this, url::quirks::password)
    }

    fn set_password(cx: &mut Cx<'_>, this: ObjectId, value: String) -> Fallible<()> {
        edit(cx, this, |u| {
            let _ = url::quirks::set_password(u, &value);
        })
    }

    fn host(cx: &mut Cx<'_>, this: ObjectId) -> Fallible<String> {
        read(cx, this, url::quirks::host)
    }

    fn set_host(cx: &mut Cx<'_>, this: ObjectId, value: String) -> Fallible<()> {
        edit(cx, this, |u| {
            let _ = url::quirks::set_host(u, &value);
        })
    }

    fn hostname(cx: &mut Cx<'_>, this: ObjectId) -> Fallible<String> {
        read(cx, this, url::quirks::hostname)
    }

    fn set_hostname(cx: &mut Cx<'_>, this: ObjectId, value: String) -> Fallible<()> {
        edit(cx, this, |u| {
            let _ = url::quirks::set_hostname(u, &value);
        })
    }

    fn port(cx: &mut Cx<'_>, this: ObjectId) -> Fallible<String> {
        read(cx, this, url::quirks::port)
    }

    fn set_port(cx: &mut Cx<'_>, this: ObjectId, value: String) -> Fallible<()> {
        edit(cx, this, |u| {
            let _ = url::quirks::set_port(u, &value);
        })
    }

    fn pathname(cx: &mut Cx<'_>, this: ObjectId) -> Fallible<String> {
        read(cx, this, url::quirks::pathname)
    }

    fn set_pathname(cx: &mut Cx<'_>, this: ObjectId, value: String) -> Fallible<()> {
        edit(cx, this, |u| url::quirks::set_pathname(u, &value))
    }

    fn search(cx: &mut Cx<'_>, this: ObjectId) -> Fallible<String> {
        read(cx, this, url::quirks::search)
    }

    fn set_search(cx: &mut Cx<'_>, this: ObjectId, value: String) -> Fallible<()> {
        edit(cx, this, |u| url::quirks::set_search(u, &value))
    }

    fn search_params(cx: &mut Cx<'_>, this: ObjectId) -> Fallible<ObjectId> {
        let (existing, query) = with_url(cx, this, |u| {
            (
                u.search_params,
                u.url.query().unwrap_or_default().to_string(),
            )
        })?;
        if let Some(id) = existing.filter(|&id| cx.page.object_exists(id)) {
            return Ok(id);
        }
        let params = cx.page.alloc(SearchParamsObject {
            list: parse_query(&query),
            url: Some(this),
        });
        with_url(cx, this, |u| u.search_params = Some(params))?;
        Ok(params)
    }

    fn hash(cx: &mut Cx<'_>, this: ObjectId) -> Fallible<String> {
        read(cx, this, url::quirks::hash)
    }

    fn set_hash(cx: &mut Cx<'_>, this: ObjectId, value: String) -> Fallible<()> {
        edit(cx, this, |u| url::quirks::set_hash(u, &value))
    }

    fn to_json(cx: &mut Cx<'_>, this: ObjectId) -> Fallible<String> {
        read(cx, this, url::quirks::href)
    }

    fn constructor(cx: &mut Cx<'_>, url: String, base: Option<String>) -> Fallible<ObjectId> {
        let parsed = parse_url(&url, base.as_deref()).ok_or_else(|| invalid_url(&url))?;
        Ok(cx.page.alloc(UrlObject {
            url: parsed,
            search_params: None,
        }))
    }
}

/// The `application/x-www-form-urlencoded` form of a `URLSearchParams`.
pub(crate) fn serialized_params(cx: &Cx<'_>, this: ObjectId) -> Fallible<String> {
    with_params(cx, this, |list| serialize_query(list))
}

fn with_params<R>(
    cx: &Cx<'_>,
    this: ObjectId,
    f: impl FnOnce(&mut Vec<(String, String)>) -> R,
) -> Fallible<R> {
    cx.page
        .with::<SearchParamsObject, _>(this, |p| f(&mut p.list))
}

/// Edits the list and writes it back to the associated URL's query.
fn update<R>(
    cx: &Cx<'_>,
    this: ObjectId,
    f: impl FnOnce(&mut Vec<(String, String)>) -> R,
) -> Fallible<R> {
    let (result, url, query) = cx.page.with::<SearchParamsObject, _>(this, |p| {
        let result = f(&mut p.list);
        (result, p.url, serialize_query(&p.list))
    })?;
    if let Some(url) = url {
        // The URL object may be gone already; the list then stands alone.
        let _ = cx.page.with::<UrlObject, _>(url, |u| {
            u.url
                .set_query((!query.is_empty()).then_some(query.as_str()));
        });
    }
    Ok(result)
}

impl web::URLSearchParamsImpl for Web {
    fn size(cx: &mut Cx<'_>, this: ObjectId) -> Fallible<u32> {
        with_params(cx, this, |list| list.len() as u32)
    }

    fn append(cx: &mut Cx<'_>, this: ObjectId, name: String, value: String) -> Fallible<()> {
        update(cx, this, |list| list.push((name, value)))
    }

    fn delete(
        cx: &mut Cx<'_>,
        this: ObjectId,
        name: String,
        value: Option<String>,
    ) -> Fallible<()> {
        update(cx, this, |list| {
            list.retain(|(n, v)| !(*n == name && value.as_ref().is_none_or(|x| x == v)));
        })
    }

    fn get(cx: &mut Cx<'_>, this: ObjectId, name: String) -> Fallible<Option<String>> {
        with_params(cx, this, |list| {
            list.iter()
                .find(|(n, _)| *n == name)
                .map(|(_, v)| v.clone())
        })
    }

    fn get_all(cx: &mut Cx<'_>, this: ObjectId, name: String) -> Fallible<Vec<String>> {
        with_params(cx, this, |list| {
            list.iter()
                .filter(|(n, _)| *n == name)
                .map(|(_, v)| v.clone())
                .collect()
        })
    }

    fn has(cx: &mut Cx<'_>, this: ObjectId, name: String, value: Option<String>) -> Fallible<bool> {
        with_params(cx, this, |list| {
            list.iter()
                .any(|(n, v)| *n == name && value.as_ref().is_none_or(|x| x == v))
        })
    }

    fn set(cx: &mut Cx<'_>, this: ObjectId, name: String, value: String) -> Fallible<()> {
        update(cx, this, |list| {
            match list.iter().position(|(n, _)| *n == name) {
                Some(first) => {
                    list[first].1 = value;
                    let mut index = 0;
                    list.retain(|(n, _)| {
                        let keep = index <= first || *n != name;
                        index += 1;
                        keep
                    });
                }
                None => list.push((name, value)),
            }
        })
    }

    fn sort(cx: &mut Cx<'_>, this: ObjectId) -> Fallible<()> {
        update(cx, this, |list| {
            // Stable, by the names' UTF-16 code units.
            list.sort_by(|a, b| a.0.encode_utf16().cmp(b.0.encode_utf16()));
        })
    }

    fn constructor(cx: &mut Cx<'_>, init: ParamsInit) -> Fallible<ObjectId> {
        let list = match init {
            ParamsInit::StringSequenceSequence(pairs) => {
                let mut list = Vec::with_capacity(pairs.len());
                for pair in pairs {
                    let [name, value] = <[String; 2]>::try_from(pair).map_err(|_| {
                        Exception::type_error("Each pair must contain exactly two strings")
                    })?;
                    list.push((name, value));
                }
                list
            }
            ParamsInit::StringStringRecord(record) => record,
            ParamsInit::String(s) => parse_query(s.strip_prefix('?').unwrap_or(&s)),
        };
        Ok(cx.page.alloc(SearchParamsObject { list, url: None }))
    }

    fn iterate(cx: &mut Cx<'_>, this: ObjectId) -> Fallible<Vec<(String, String)>> {
        with_params(cx, this, |list| list.clone())
    }

    fn stringify(cx: &mut Cx<'_>, this: ObjectId) -> Fallible<String> {
        with_params(cx, this, |list| serialize_query(list))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn query_round_trip() {
        let list = parse_query("a=1&b=x+y&a=%E4%B8%AD");
        assert_eq!(
            list,
            vec![
                ("a".to_string(), "1".to_string()),
                ("b".to_string(), "x y".to_string()),
                ("a".to_string(), "中".to_string()),
            ]
        );
        assert_eq!(serialize_query(&list), "a=1&b=x+y&a=%E4%B8%AD");
    }

    #[test]
    fn resolves_against_base() {
        let url = parse_url("../x?q=1", Some("https://example.com/a/b/c")).unwrap();
        assert_eq!(url.as_str(), "https://example.com/a/x?q=1");
        assert!(parse_url("/relative", None).is_none());
        assert!(parse_url("x", Some("not a url")).is_none());
    }
}

use std::any::Any;

use crate::bstr::BStr;

impl crate::Repository {
    /// Produce configuration suitable for `url`, as differentiated by its protocol/scheme, to be passed to a transport instance via
    /// [configure()][git_transport::client::TransportWithoutIO::configure()] (via `&**config` to pass the contained `Any` and not the `Box`).
    /// `None` is returned if there is no known configuration. If `remote_name` is not `None`, the remote's name may contribute to
    /// configuration overrides, typically for the HTTP transport.
    ///
    /// Note that the caller may cast the instance themselves to modify it before passing it on.
    ///
    /// For transports that support proxy authentication, the
    /// [default authentication method](crate::config::Snapshot::credential_helpers()) will be used with the url of the proxy
    /// if it contains a user name.
    #[cfg_attr(
        not(any(
            feature = "blocking-http-transport-reqwest",
            feature = "blocking-http-transport-curl"
        )),
        allow(unused_variables)
    )]
    pub fn transport_options<'a>(
        &self,
        url: impl Into<&'a BStr>,
        remote_name: Option<&BStr>,
    ) -> Result<Option<Box<dyn Any>>, crate::config::transport::Error> {
        let url = git_url::parse(url.into())?;
        use git_url::Scheme::*;

        match &url.scheme {
            Http | Https => {
                #[cfg(not(any(
                    feature = "blocking-http-transport-reqwest",
                    feature = "blocking-http-transport-curl"
                )))]
                {
                    Ok(None)
                }
                #[cfg(any(
                    feature = "blocking-http-transport-reqwest",
                    feature = "blocking-http-transport-curl"
                ))]
                {
                    use std::borrow::Cow;
                    use std::sync::{Arc, Mutex};

                    use git_transport::client::http;
                    use git_transport::client::http::options::ProxyAuthMethod;

                    use crate::{
                        bstr::ByteVec,
                        config::cache::util::{ApplyLeniency, ApplyLeniencyDefault},
                    };
                    fn try_cow_to_string(
                        v: Cow<'_, BStr>,
                        lenient: bool,
                        key: impl Into<Cow<'static, BStr>>,
                    ) -> Result<Option<String>, crate::config::transport::Error> {
                        Vec::from(v.into_owned())
                            .into_string()
                            .map(Some)
                            .map_err(|err| crate::config::transport::Error::IllformedUtf8 {
                                source: err,
                                key: key.into(),
                            })
                            .with_leniency(lenient)
                    }

                    fn integer<T>(
                        config: &git_config::File<'static>,
                        lenient: bool,
                        key: &'static str,
                        kind: &'static str,
                        filter: fn(&git_config::file::Metadata) -> bool,
                        default: T,
                    ) -> Result<T, crate::config::transport::Error>
                    where
                        T: TryFrom<i64>,
                    {
                        Ok(integer_opt(config, lenient, key, kind, filter)?.unwrap_or(default))
                    }

                    fn integer_opt<T>(
                        config: &git_config::File<'static>,
                        lenient: bool,
                        key: &'static str,
                        kind: &'static str,
                        mut filter: fn(&git_config::file::Metadata) -> bool,
                    ) -> Result<Option<T>, crate::config::transport::Error>
                    where
                        T: TryFrom<i64>,
                    {
                        let git_config::parse::Key {
                            section_name,
                            subsection_name,
                            value_name,
                        } = git_config::parse::key(key).expect("valid key statically known");
                        config
                            .integer_filter(section_name, subsection_name, value_name, &mut filter)
                            .transpose()
                            .map_err(|err| crate::config::transport::Error::ConfigValue { source: err, key })
                            .with_leniency(lenient)?
                            .map(|integer| {
                                integer
                                    .try_into()
                                    .map_err(|_| crate::config::transport::Error::InvalidInteger {
                                        actual: integer,
                                        key,
                                        kind,
                                    })
                            })
                            .transpose()
                            .with_leniency(lenient)
                    }

                    fn proxy_auth_method(
                        value_and_key: Option<(Cow<'_, BStr>, Cow<'static, BStr>)>,
                        lenient: bool,
                    ) -> Result<ProxyAuthMethod, crate::config::transport::Error> {
                        let value = value_and_key
                            .and_then(|(v, k)| {
                                try_cow_to_string(v, lenient, k.clone())
                                    .map(|v| v.map(|v| (v, k)))
                                    .transpose()
                            })
                            .transpose()?
                            .map(|(method, key)| {
                                Ok(match method.as_str() {
                                    "anyauth" => ProxyAuthMethod::AnyAuth,
                                    "basic" => ProxyAuthMethod::Basic,
                                    "digest" => ProxyAuthMethod::Digest,
                                    "negotiate" => ProxyAuthMethod::Negotiate,
                                    "ntlm" => ProxyAuthMethod::Ntlm,
                                    _ => {
                                        return Err(crate::config::transport::http::Error::InvalidProxyAuthMethod {
                                            value: method,
                                            key,
                                        })
                                    }
                                })
                            })
                            .transpose()?
                            .unwrap_or_default();
                        Ok(value)
                    }

                    fn proxy(
                        value: Option<(Cow<'_, BStr>, Cow<'static, BStr>)>,
                        lenient: bool,
                    ) -> Result<Option<String>, crate::config::transport::Error> {
                        Ok(value
                            .and_then(|(v, k)| try_cow_to_string(v, lenient, k.clone()).transpose())
                            .transpose()?
                            .map(|mut proxy| {
                                if !proxy.trim().is_empty() && !proxy.contains("://") {
                                    proxy.insert_str(0, "http://");
                                    proxy
                                } else {
                                    proxy
                                }
                            }))
                    }

                    let mut opts = http::Options::default();
                    let config = &self.config.resolved;
                    let mut trusted_only = self.filter_config_section();
                    let lenient = self.config.lenient_config;
                    opts.extra_headers = {
                        let mut headers = Vec::new();
                        for header in config
                            .strings_filter("http", None, "extraHeader", &mut trusted_only)
                            .unwrap_or_default()
                            .into_iter()
                            .map(|v| try_cow_to_string(v, lenient, Cow::Borrowed("http.extraHeader".into())))
                        {
                            let header = header?;
                            if let Some(header) = header {
                                headers.push(header);
                            }
                        }
                        if let Some(empty_pos) = headers.iter().rev().position(|h| h.is_empty()) {
                            headers.drain(..headers.len() - empty_pos);
                        }
                        headers
                    };

                    if let Some(follow_redirects) =
                        config.string_filter("http", None, "followRedirects", &mut trusted_only)
                    {
                        opts.follow_redirects = if follow_redirects.as_ref() == "initial" {
                            http::options::FollowRedirects::Initial
                        } else if git_config::Boolean::try_from(follow_redirects)
                            .map_err(|err| crate::config::transport::Error::ConfigValue {
                                source: err,
                                key: "http.followRedirects",
                            })
                            .with_lenient_default(lenient)?
                            .0
                        {
                            http::options::FollowRedirects::All
                        } else {
                            http::options::FollowRedirects::None
                        };
                    }

                    opts.low_speed_time_seconds =
                        integer(config, lenient, "http.lowSpeedTime", "u64", trusted_only, 0)?;
                    opts.low_speed_limit_bytes_per_second =
                        integer(config, lenient, "http.lowSpeedLimit", "u32", trusted_only, 0)?;
                    opts.proxy = proxy(
                        remote_name
                            .and_then(|name| {
                                config
                                    .string_filter("remote", Some(name), "proxy", &mut trusted_only)
                                    .map(|v| (v, Cow::Owned(format!("remote.{name}.proxy").into())))
                            })
                            .or_else(|| {
                                config
                                    .string_filter("http", None, "proxy", &mut trusted_only)
                                    .map(|v| (v, Cow::Borrowed("http.proxy".into())))
                            }),
                        lenient,
                    )?;
                    opts.proxy_auth_method = proxy_auth_method(
                        remote_name
                            .and_then(|name| {
                                config
                                    .string_filter("remote", Some(name), "proxyAuthMethod", &mut trusted_only)
                                    .map(|v| (v, Cow::Owned(format!("remote.{name}.proxyAuthMethod").into())))
                            })
                            .or_else(|| {
                                config
                                    .string_filter("http", None, "proxyAuthMethod", &mut trusted_only)
                                    .map(|v| (v, Cow::Borrowed("http.proxyAuthMethod".into())))
                            }),
                        lenient,
                    )?;
                    opts.proxy_authenticate = opts
                        .proxy
                        .as_deref()
                        .map(|url| git_url::parse(url.into()))
                        .transpose()?
                        .filter(|url| url.user().is_some())
                        .map(|url| -> Result<_, crate::config::transport::http::Error> {
                            let (mut cascade, action_with_normalized_url, prompt_opts) =
                                self.config_snapshot().credential_helpers(url)?;
                            Ok((
                                action_with_normalized_url,
                                Arc::new(Mutex::new(move |action| cascade.invoke(action, prompt_opts.clone())))
                                    as Arc<Mutex<git_transport::client::http::AuthenticateFn>>,
                            ))
                        })
                        .transpose()?;
                    opts.connect_timeout =
                        integer_opt(config, lenient, "gitoxide.http.connectTimeout", "u64", trusted_only)?
                            .map(std::time::Duration::from_millis);
                    opts.user_agent = config
                        .string_filter("http", None, "userAgent", &mut trusted_only)
                        .and_then(|v| try_cow_to_string(v, lenient, Cow::Borrowed("http.userAgent".into())).transpose())
                        .transpose()?
                        .or_else(|| Some(crate::env::agent().into()));

                    Ok(Some(Box::new(opts)))
                }
            }
            File | Git | Ssh | Ext(_) => Ok(None),
        }
    }
}
//! The macro behind every string vocabulary on the wire.
//!
//! One declaration produces the variants, the token table, and the serde impls, so the token a
//! reader branches on and the token the writer emits cannot drift apart the way a `rename_all`
//! attribute and a hand-written `token()` can.

/// Declare a wire vocabulary. `pub open enum` grows an `Other(String)` arm that round-trips a token
/// this build has never heard of (ARCHITECTURE §2.3.2, A-104); `pub enum` refuses one.
macro_rules! vocabulary {
    (
        $(#[$meta:meta])*
        pub open enum $name:ident {
            $( $(#[$vmeta:meta])* $variant:ident = $token:literal ),+ $(,)?
        }
    ) => {
        $(#[$meta])*
        #[derive(Clone, Debug, PartialEq, Eq, Hash)]
        pub enum $name {
            $( $(#[$vmeta])* $variant, )+
            /// A token minted after this build: kept verbatim rather than collapsed into a
            /// neighbour, because dropping a variant changes meaning.
            Other(String),
        }

        impl $name {
            /// The tokens this build knows, in declaration order.
            pub const TOKENS: &'static [&'static str] = &[$($token),+];

            /// This value's wire token.
            pub fn token(&self) -> &str {
                match self {
                    $( $name::$variant => $token, )+
                    $name::Other(token) => token.as_str(),
                }
            }

            /// The value for `token`, or [`Self::Other`] when this build does not know it.
            pub fn from_token(token: &str) -> $name {
                match token {
                    $( $token => $name::$variant, )+
                    other => $name::Other(other.to_string()),
                }
            }

            /// Whether this build recognizes the token.
            pub fn is_known(&self) -> bool {
                !matches!(self, $name::Other(_))
            }
        }

        impl ::std::fmt::Display for $name {
            fn fmt(&self, f: &mut ::std::fmt::Formatter<'_>) -> ::std::fmt::Result {
                f.write_str(self.token())
            }
        }

        impl ::serde::Serialize for $name {
            fn serialize<S: ::serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
                s.serialize_str(self.token())
            }
        }

        impl<'de> ::serde::Deserialize<'de> for $name {
            fn deserialize<D: ::serde::Deserializer<'de>>(d: D) -> Result<$name, D::Error> {
                let token = <String as ::serde::Deserialize>::deserialize(d)?;
                Ok($name::from_token(&token))
            }
        }
    };

    (
        $(#[$meta:meta])*
        pub enum $name:ident {
            $( $(#[$vmeta:meta])* $variant:ident = $token:literal ),+ $(,)?
        }
    ) => {
        $(#[$meta])*
        #[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
        pub enum $name {
            $( $(#[$vmeta])* $variant, )+
        }

        impl $name {
            /// Every value, in declaration order.
            pub const ALL: &'static [$name] = &[$($name::$variant),+];

            /// Every token, in declaration order.
            pub const TOKENS: &'static [&'static str] = &[$($token),+];

            /// This value's wire token.
            pub const fn token(self) -> &'static str {
                match self {
                    $( $name::$variant => $token, )+
                }
            }

            /// The value for `token`, or `None`: a closed vocabulary refuses what it does not know.
            pub fn from_token(token: &str) -> Option<$name> {
                match token {
                    $( $token => Some($name::$variant), )+
                    _ => None,
                }
            }
        }

        impl ::std::fmt::Display for $name {
            fn fmt(&self, f: &mut ::std::fmt::Formatter<'_>) -> ::std::fmt::Result {
                f.write_str(self.token())
            }
        }

        impl ::serde::Serialize for $name {
            fn serialize<S: ::serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
                s.serialize_str(self.token())
            }
        }

        impl<'de> ::serde::Deserialize<'de> for $name {
            fn deserialize<D: ::serde::Deserializer<'de>>(d: D) -> Result<$name, D::Error> {
                let token = <String as ::serde::Deserialize>::deserialize(d)?;
                $name::from_token(&token).ok_or_else(|| {
                    ::serde::de::Error::unknown_variant(&token, $name::TOKENS)
                })
            }
        }
    };
}

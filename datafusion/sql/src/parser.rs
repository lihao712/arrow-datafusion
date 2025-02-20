// Licensed to the Apache Software Foundation (ASF) under one
// or more contributor license agreements.  See the NOTICE file
// distributed with this work for additional information
// regarding copyright ownership.  The ASF licenses this file
// to you under the Apache License, Version 2.0 (the
// "License"); you may not use this file except in compliance
// with the License.  You may obtain a copy of the License at
//
//   http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing,
// software distributed under the License is distributed on an
// "AS IS" BASIS, WITHOUT WARRANTIES OR CONDITIONS OF ANY
// KIND, either express or implied.  See the License for the
// specific language governing permissions and limitations
// under the License.

//! DataFusion SQL Parser based on [`sqlparser`]

use datafusion_common::parsers::CompressionTypeVariant;
use sqlparser::ast::OrderByExpr;
use sqlparser::{
    ast::{
        ColumnDef, ColumnOptionDef, ObjectName, Statement as SQLStatement,
        TableConstraint,
    },
    dialect::{keywords::Keyword, Dialect, GenericDialect},
    parser::{Parser, ParserError},
    tokenizer::{Token, TokenWithLocation, Tokenizer},
};
use std::{collections::HashMap, str::FromStr};
use std::{collections::VecDeque, fmt};

// Use `Parser::expected` instead, if possible
macro_rules! parser_err {
    ($MSG:expr) => {
        Err(ParserError::ParserError($MSG.to_string()))
    };
}

fn parse_file_type(s: &str) -> Result<String, ParserError> {
    Ok(s.to_uppercase())
}

/// DataFusion extension DDL for `CREATE EXTERNAL TABLE`
///
/// Syntax:
///
/// ```text
/// CREATE EXTERNAL TABLE
/// [ IF NOT EXISTS ]
/// <TABLE_NAME>[ (<column_definition>) ]
/// STORED AS <file_type>
/// [ WITH HEADER ROW ]
/// [ DELIMITER <char> ]
/// [ COMPRESSION TYPE <GZIP | BZIP2 | XZ | ZSTD> ]
/// [ PARTITIONED BY (<column list>) ]
/// [ WITH ORDER (<ordered column list>)
/// [ OPTIONS (<key_value_list>) ]
/// LOCATION <literal>
///
/// <column_definition> := (<column_name> <data_type>, ...)
///
/// <column_list> := (<column_name>, ...)
///
/// <ordered_column_list> := (<column_name> <sort_clause>, ...)
///
/// <key_value_list> := (<literal> <literal, <literal> <literal>, ...)
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CreateExternalTable {
    /// Table name
    pub name: String,
    /// Optional schema
    pub columns: Vec<ColumnDef>,
    /// File type (Parquet, NDJSON, CSV, etc)
    pub file_type: String,
    /// CSV Header row?
    pub has_header: bool,
    /// User defined delimiter for CSVs
    pub delimiter: char,
    /// Path to file
    pub location: String,
    /// Partition Columns
    pub table_partition_cols: Vec<String>,
    /// Ordered expressions
    pub order_exprs: Vec<OrderByExpr>,
    /// Option to not error if table already exists
    pub if_not_exists: bool,
    /// File compression type (GZIP, BZIP2, XZ)
    pub file_compression_type: CompressionTypeVariant,
    /// Table(provider) specific options
    pub options: HashMap<String, String>,
}

impl fmt::Display for CreateExternalTable {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "CREATE EXTERNAL TABLE ")?;
        if self.if_not_exists {
            write!(f, "IF NOT EXSISTS ")?;
        }
        write!(f, "{} ", self.name)?;
        write!(f, "STORED AS {} ", self.file_type)?;
        write!(f, "LOCATION {} ", self.location)
    }
}

/// DataFusion extension DDL for `DESCRIBE TABLE`
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DescribeTableStmt {
    /// Table name
    pub table_name: ObjectName,
}

/// DataFusion Statement representations.
///
/// Tokens parsed by [`DFParser`] are converted into these values.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Statement {
    /// ANSI SQL AST node
    Statement(Box<SQLStatement>),
    /// Extension: `CREATE EXTERNAL TABLE`
    CreateExternalTable(CreateExternalTable),
    /// Extension: `DESCRIBE TABLE`
    DescribeTableStmt(DescribeTableStmt),
}

/// DataFusion SQL Parser based on [`sqlparser`]
///
/// This parser handles DataFusion specific statements, delegating to
/// [`Parser`](sqlparser::parser::Parser) for other SQL statements.
pub struct DFParser<'a> {
    parser: Parser<'a>,
}

impl<'a> DFParser<'a> {
    /// Create a new parser for the specified tokens using the
    /// [`GenericDialect`].
    pub fn new(sql: &str) -> Result<Self, ParserError> {
        let dialect = &GenericDialect {};
        DFParser::new_with_dialect(sql, dialect)
    }

    /// Create a new parser for the specified tokens with the
    /// specified dialect.
    pub fn new_with_dialect(
        sql: &str,
        dialect: &'a dyn Dialect,
    ) -> Result<Self, ParserError> {
        let mut tokenizer = Tokenizer::new(dialect, sql);
        let tokens = tokenizer.tokenize()?;

        Ok(DFParser {
            parser: Parser::new(dialect).with_tokens(tokens),
        })
    }

    /// Parse a sql string into one or [`Statement`]s using the
    /// [`GenericDialect`].
    pub fn parse_sql(sql: &str) -> Result<VecDeque<Statement>, ParserError> {
        let dialect = &GenericDialect {};
        DFParser::parse_sql_with_dialect(sql, dialect)
    }

    /// Parse a SQL string and produce one or more [`Statement`]s with
    /// with the specified dialect.
    pub fn parse_sql_with_dialect(
        sql: &str,
        dialect: &dyn Dialect,
    ) -> Result<VecDeque<Statement>, ParserError> {
        let mut parser = DFParser::new_with_dialect(sql, dialect)?;
        let mut stmts = VecDeque::new();
        let mut expecting_statement_delimiter = false;
        loop {
            // ignore empty statements (between successive statement delimiters)
            while parser.parser.consume_token(&Token::SemiColon) {
                expecting_statement_delimiter = false;
            }

            if parser.parser.peek_token() == Token::EOF {
                break;
            }
            if expecting_statement_delimiter {
                return parser.expected("end of statement", parser.parser.peek_token());
            }

            let statement = parser.parse_statement()?;
            stmts.push_back(statement);
            expecting_statement_delimiter = true;
        }
        Ok(stmts)
    }

    /// Report an unexpected token
    fn expected<T>(
        &self,
        expected: &str,
        found: TokenWithLocation,
    ) -> Result<T, ParserError> {
        parser_err!(format!("Expected {expected}, found: {found}"))
    }

    /// Parse a new expression
    pub fn parse_statement(&mut self) -> Result<Statement, ParserError> {
        match self.parser.peek_token().token {
            Token::Word(w) => {
                match w.keyword {
                    Keyword::CREATE => {
                        // move one token forward
                        self.parser.next_token();
                        // use custom parsing
                        self.parse_create()
                    }
                    Keyword::DESCRIBE => {
                        // move one token forward
                        self.parser.next_token();
                        // use custom parsing
                        self.parse_describe()
                    }
                    _ => {
                        // use the native parser
                        Ok(Statement::Statement(Box::from(
                            self.parser.parse_statement()?,
                        )))
                    }
                }
            }
            _ => {
                // use the native parser
                Ok(Statement::Statement(Box::from(
                    self.parser.parse_statement()?,
                )))
            }
        }
    }

    /// Parse a SQL `DESCRIBE` statement
    pub fn parse_describe(&mut self) -> Result<Statement, ParserError> {
        let table_name = self.parser.parse_object_name()?;
        Ok(Statement::DescribeTableStmt(DescribeTableStmt {
            table_name,
        }))
    }

    /// Parse a SQL `CREATE` statement handling `CREATE EXTERNAL TABLE`
    pub fn parse_create(&mut self) -> Result<Statement, ParserError> {
        if self.parser.parse_keyword(Keyword::EXTERNAL) {
            self.parse_create_external_table()
        } else {
            Ok(Statement::Statement(Box::from(self.parser.parse_create()?)))
        }
    }

    fn parse_partitions(&mut self) -> Result<Vec<String>, ParserError> {
        let mut partitions: Vec<String> = vec![];
        if !self.parser.consume_token(&Token::LParen)
            || self.parser.consume_token(&Token::RParen)
        {
            return Ok(partitions);
        }

        loop {
            if let Token::Word(_) = self.parser.peek_token().token {
                let identifier = self.parser.parse_identifier()?;
                partitions.push(identifier.to_string());
            } else {
                return self.expected("partition name", self.parser.peek_token());
            }
            let comma = self.parser.consume_token(&Token::Comma);
            if self.parser.consume_token(&Token::RParen) {
                // allow a trailing comma, even though it's not in standard
                break;
            } else if !comma {
                return self.expected(
                    "',' or ')' after partition definition",
                    self.parser.peek_token(),
                );
            }
        }
        Ok(partitions)
    }

    /// Parse the ordering clause of a `CREATE EXTERNAL TABLE` SQL statement
    pub fn parse_order_by_exprs(&mut self) -> Result<Vec<OrderByExpr>, ParserError> {
        let mut values = vec![];
        self.parser.expect_token(&Token::LParen)?;
        loop {
            values.push(self.parse_order_by_expr()?);
            if !self.parser.consume_token(&Token::Comma) {
                self.parser.expect_token(&Token::RParen)?;
                return Ok(values);
            }
        }
    }

    /// Parse an ORDER BY sub-expression optionally followed by ASC or DESC.
    pub fn parse_order_by_expr(&mut self) -> Result<OrderByExpr, ParserError> {
        let expr = self.parser.parse_expr()?;

        let asc = if self.parser.parse_keyword(Keyword::ASC) {
            Some(true)
        } else if self.parser.parse_keyword(Keyword::DESC) {
            Some(false)
        } else {
            None
        };

        let nulls_first = if self
            .parser
            .parse_keywords(&[Keyword::NULLS, Keyword::FIRST])
        {
            Some(true)
        } else if self.parser.parse_keywords(&[Keyword::NULLS, Keyword::LAST]) {
            Some(false)
        } else {
            None
        };

        Ok(OrderByExpr {
            expr,
            asc,
            nulls_first,
        })
    }

    // This is a copy of the equivalent implementation in sqlparser.
    fn parse_columns(
        &mut self,
    ) -> Result<(Vec<ColumnDef>, Vec<TableConstraint>), ParserError> {
        let mut columns = vec![];
        let mut constraints = vec![];
        if !self.parser.consume_token(&Token::LParen)
            || self.parser.consume_token(&Token::RParen)
        {
            return Ok((columns, constraints));
        }

        loop {
            if let Some(constraint) = self.parser.parse_optional_table_constraint()? {
                constraints.push(constraint);
            } else if let Token::Word(_) = self.parser.peek_token().token {
                let column_def = self.parse_column_def()?;
                columns.push(column_def);
            } else {
                return self.expected(
                    "column name or constraint definition",
                    self.parser.peek_token(),
                );
            }
            let comma = self.parser.consume_token(&Token::Comma);
            if self.parser.consume_token(&Token::RParen) {
                // allow a trailing comma, even though it's not in standard
                break;
            } else if !comma {
                return self.expected(
                    "',' or ')' after column definition",
                    self.parser.peek_token(),
                );
            }
        }

        Ok((columns, constraints))
    }

    fn parse_column_def(&mut self) -> Result<ColumnDef, ParserError> {
        let name = self.parser.parse_identifier()?;
        let data_type = self.parser.parse_data_type()?;
        let collation = if self.parser.parse_keyword(Keyword::COLLATE) {
            Some(self.parser.parse_object_name()?)
        } else {
            None
        };
        let mut options = vec![];
        loop {
            if self.parser.parse_keyword(Keyword::CONSTRAINT) {
                let name = Some(self.parser.parse_identifier()?);
                if let Some(option) = self.parser.parse_optional_column_option()? {
                    options.push(ColumnOptionDef { name, option });
                } else {
                    return self.expected(
                        "constraint details after CONSTRAINT <name>",
                        self.parser.peek_token(),
                    );
                }
            } else if let Some(option) = self.parser.parse_optional_column_option()? {
                options.push(ColumnOptionDef { name: None, option });
            } else {
                break;
            };
        }
        Ok(ColumnDef {
            name,
            data_type,
            collation,
            options,
        })
    }

    fn parse_create_external_table(&mut self) -> Result<Statement, ParserError> {
        self.parser.expect_keyword(Keyword::TABLE)?;
        let if_not_exists =
            self.parser
                .parse_keywords(&[Keyword::IF, Keyword::NOT, Keyword::EXISTS]);
        let table_name = self.parser.parse_object_name()?;
        let (columns, _) = self.parse_columns()?;

        #[derive(Default)]
        struct Builder {
            file_type: Option<String>,
            location: Option<String>,
            has_header: Option<bool>,
            delimiter: Option<char>,
            file_compression_type: Option<CompressionTypeVariant>,
            table_partition_cols: Option<Vec<String>>,
            order_exprs: Option<Vec<OrderByExpr>>,
            options: Option<HashMap<String, String>>,
        }
        let mut builder = Builder::default();

        fn ensure_not_set<T>(field: &Option<T>, name: &str) -> Result<(), ParserError> {
            if field.is_some() {
                return Err(ParserError::ParserError(format!(
                    "{name} specified more than once",
                )));
            }
            Ok(())
        }

        loop {
            if self.parser.parse_keyword(Keyword::STORED) {
                self.parser.expect_keyword(Keyword::AS)?;
                ensure_not_set(&builder.file_type, "STORED AS")?;
                builder.file_type = Some(self.parse_file_format()?);
            } else if self.parser.parse_keyword(Keyword::LOCATION) {
                ensure_not_set(&builder.location, "LOCATION")?;
                builder.location = Some(self.parser.parse_literal_string()?);
            } else if self.parser.parse_keyword(Keyword::WITH) {
                if self.parser.parse_keyword(Keyword::ORDER) {
                    ensure_not_set(&builder.order_exprs, "WITH ORDER")?;
                    builder.order_exprs = Some(self.parse_order_by_exprs()?);
                } else {
                    self.parser.expect_keyword(Keyword::HEADER)?;
                    self.parser.expect_keyword(Keyword::ROW)?;
                    ensure_not_set(&builder.has_header, "WITH HEADER ROW")?;
                    builder.has_header = Some(true);
                }
            } else if self.parser.parse_keyword(Keyword::DELIMITER) {
                ensure_not_set(&builder.delimiter, "DELIMITER")?;
                builder.delimiter = Some(self.parse_delimiter()?);
            } else if self.parser.parse_keyword(Keyword::COMPRESSION) {
                self.parser.expect_keyword(Keyword::TYPE)?;
                ensure_not_set(&builder.file_compression_type, "COMPRESSION TYPE")?;
                builder.file_compression_type = Some(self.parse_file_compression_type()?);
            } else if self.parser.parse_keyword(Keyword::PARTITIONED) {
                self.parser.expect_keyword(Keyword::BY)?;
                ensure_not_set(&builder.table_partition_cols, "PARTITIONED BY")?;
                builder.table_partition_cols = Some(self.parse_partitions()?)
            } else if self.parser.parse_keyword(Keyword::OPTIONS) {
                ensure_not_set(&builder.options, "OPTIONS")?;
                builder.options = Some(self.parse_options()?);
            } else {
                break;
            }
        }

        // Validations: location and file_type are required
        if builder.file_type.is_none() {
            return Err(ParserError::ParserError(
                "Missing STORED AS clause in CREATE EXTERNAL TABLE statement".into(),
            ));
        }
        if builder.location.is_none() {
            return Err(ParserError::ParserError(
                "Missing LOCATION clause in CREATE EXTERNAL TABLE statement".into(),
            ));
        }

        let create = CreateExternalTable {
            name: table_name.to_string(),
            columns,
            file_type: builder.file_type.unwrap(),
            has_header: builder.has_header.unwrap_or(false),
            delimiter: builder.delimiter.unwrap_or(','),
            location: builder.location.unwrap(),
            table_partition_cols: builder.table_partition_cols.unwrap_or(vec![]),
            order_exprs: builder.order_exprs.unwrap_or(vec![]),
            if_not_exists,
            file_compression_type: builder
                .file_compression_type
                .unwrap_or(CompressionTypeVariant::UNCOMPRESSED),
            options: builder.options.unwrap_or(HashMap::new()),
        };
        Ok(Statement::CreateExternalTable(create))
    }

    /// Parses the set of valid formats
    fn parse_file_format(&mut self) -> Result<String, ParserError> {
        let token = self.parser.next_token();
        match &token.token {
            Token::Word(w) => parse_file_type(&w.value),
            _ => self.expected("one of PARQUET, NDJSON, or CSV", token),
        }
    }

    /// Parses the set of
    fn parse_file_compression_type(
        &mut self,
    ) -> Result<CompressionTypeVariant, ParserError> {
        let token = self.parser.next_token();
        match &token.token {
            Token::Word(w) => CompressionTypeVariant::from_str(&w.value),
            _ => self.expected("one of GZIP, BZIP2, XZ, ZSTD", token),
        }
    }

    fn parse_options(&mut self) -> Result<HashMap<String, String>, ParserError> {
        let mut options: HashMap<String, String> = HashMap::new();
        self.parser.expect_token(&Token::LParen)?;

        loop {
            let key = self.parser.parse_literal_string()?;
            let value = self.parser.parse_literal_string()?;
            options.insert(key.to_string(), value.to_string());
            let comma = self.parser.consume_token(&Token::Comma);
            if self.parser.consume_token(&Token::RParen) {
                // allow a trailing comma, even though it's not in standard
                break;
            } else if !comma {
                return self.expected(
                    "',' or ')' after option definition",
                    self.parser.peek_token(),
                );
            }
        }
        Ok(options)
    }

    fn parse_delimiter(&mut self) -> Result<char, ParserError> {
        let token = self.parser.parse_literal_string()?;
        match token.len() {
            1 => Ok(token.chars().next().unwrap()),
            _ => Err(ParserError::TokenizerError(
                "Delimiter must be a single char".to_string(),
            )),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sqlparser::ast::Expr::Identifier;
    use sqlparser::ast::{BinaryOperator, DataType, Expr, Ident};
    use CompressionTypeVariant::UNCOMPRESSED;

    fn expect_parse_ok(sql: &str, expected: Statement) -> Result<(), ParserError> {
        let statements = DFParser::parse_sql(sql)?;
        assert_eq!(
            statements.len(),
            1,
            "Expected to parse exactly one statement"
        );
        assert_eq!(statements[0], expected);
        Ok(())
    }

    /// Parses sql and asserts that the expected error message was found
    fn expect_parse_error(sql: &str, expected_error: &str) {
        match DFParser::parse_sql(sql) {
            Ok(statements) => {
                panic!(
                    "Expected parse error for '{sql}', but was successful: {statements:?}"
                );
            }
            Err(e) => {
                let error_message = e.to_string();
                assert!(
                    error_message.contains(expected_error),
                    "Expected error '{expected_error}' not found in actual error '{error_message}'"
                );
            }
        }
    }

    fn make_column_def(name: impl Into<String>, data_type: DataType) -> ColumnDef {
        ColumnDef {
            name: Ident {
                value: name.into(),
                quote_style: None,
            },
            data_type,
            collation: None,
            options: vec![],
        }
    }

    #[test]
    fn create_external_table() -> Result<(), ParserError> {
        // positive case
        let sql = "CREATE EXTERNAL TABLE t(c1 int) STORED AS CSV LOCATION 'foo.csv'";
        let display = None;
        let expected = Statement::CreateExternalTable(CreateExternalTable {
            name: "t".into(),
            columns: vec![make_column_def("c1", DataType::Int(display))],
            file_type: "CSV".to_string(),
            has_header: false,
            delimiter: ',',
            location: "foo.csv".into(),
            table_partition_cols: vec![],
            order_exprs: vec![],
            if_not_exists: false,
            file_compression_type: UNCOMPRESSED,
            options: HashMap::new(),
        });
        expect_parse_ok(sql, expected)?;

        // positive case with delimiter
        let sql = "CREATE EXTERNAL TABLE t(c1 int) STORED AS CSV DELIMITER '|' LOCATION 'foo.csv'";
        let display = None;
        let expected = Statement::CreateExternalTable(CreateExternalTable {
            name: "t".into(),
            columns: vec![make_column_def("c1", DataType::Int(display))],
            file_type: "CSV".to_string(),
            has_header: false,
            delimiter: '|',
            location: "foo.csv".into(),
            table_partition_cols: vec![],
            order_exprs: vec![],
            if_not_exists: false,
            file_compression_type: UNCOMPRESSED,
            options: HashMap::new(),
        });
        expect_parse_ok(sql, expected)?;

        // positive case: partitioned by
        let sql = "CREATE EXTERNAL TABLE t(c1 int) STORED AS CSV PARTITIONED BY (p1, p2) LOCATION 'foo.csv'";
        let display = None;
        let expected = Statement::CreateExternalTable(CreateExternalTable {
            name: "t".into(),
            columns: vec![make_column_def("c1", DataType::Int(display))],
            file_type: "CSV".to_string(),
            has_header: false,
            delimiter: ',',
            location: "foo.csv".into(),
            table_partition_cols: vec!["p1".to_string(), "p2".to_string()],
            order_exprs: vec![],
            if_not_exists: false,
            file_compression_type: UNCOMPRESSED,
            options: HashMap::new(),
        });
        expect_parse_ok(sql, expected)?;

        // positive case: it is ok for case insensitive sql stmt with `WITH HEADER ROW` tokens
        let sqls = vec![
            "CREATE EXTERNAL TABLE t(c1 int) STORED AS CSV WITH HEADER ROW LOCATION 'foo.csv'",
            "CREATE EXTERNAL TABLE t(c1 int) STORED AS CSV with header row LOCATION 'foo.csv'"
        ];
        for sql in sqls {
            let expected = Statement::CreateExternalTable(CreateExternalTable {
                name: "t".into(),
                columns: vec![make_column_def("c1", DataType::Int(display))],
                file_type: "CSV".to_string(),
                has_header: true,
                delimiter: ',',
                location: "foo.csv".into(),
                table_partition_cols: vec![],
                order_exprs: vec![],
                if_not_exists: false,
                file_compression_type: UNCOMPRESSED,
                options: HashMap::new(),
            });
            expect_parse_ok(sql, expected)?;
        }

        // positive case: it is ok for sql stmt with `COMPRESSION TYPE GZIP` tokens
        let sqls = vec![
            ("CREATE EXTERNAL TABLE t(c1 int) STORED AS CSV COMPRESSION TYPE GZIP LOCATION 'foo.csv'", "GZIP"),
            ("CREATE EXTERNAL TABLE t(c1 int) STORED AS CSV COMPRESSION TYPE BZIP2 LOCATION 'foo.csv'", "BZIP2"),
            ("CREATE EXTERNAL TABLE t(c1 int) STORED AS CSV COMPRESSION TYPE XZ LOCATION 'foo.csv'", "XZ"),
            ("CREATE EXTERNAL TABLE t(c1 int) STORED AS CSV COMPRESSION TYPE ZSTD LOCATION 'foo.csv'", "ZSTD"),
        ];
        for (sql, file_compression_type) in sqls {
            let expected = Statement::CreateExternalTable(CreateExternalTable {
                name: "t".into(),
                columns: vec![make_column_def("c1", DataType::Int(display))],
                file_type: "CSV".to_string(),
                has_header: false,
                delimiter: ',',
                location: "foo.csv".into(),
                table_partition_cols: vec![],
                order_exprs: vec![],
                if_not_exists: false,
                file_compression_type: CompressionTypeVariant::from_str(
                    file_compression_type,
                )?,
                options: HashMap::new(),
            });
            expect_parse_ok(sql, expected)?;
        }

        // positive case: it is ok for parquet files not to have columns specified
        let sql = "CREATE EXTERNAL TABLE t STORED AS PARQUET LOCATION 'foo.parquet'";
        let expected = Statement::CreateExternalTable(CreateExternalTable {
            name: "t".into(),
            columns: vec![],
            file_type: "PARQUET".to_string(),
            has_header: false,
            delimiter: ',',
            location: "foo.parquet".into(),
            table_partition_cols: vec![],
            order_exprs: vec![],
            if_not_exists: false,
            file_compression_type: UNCOMPRESSED,
            options: HashMap::new(),
        });
        expect_parse_ok(sql, expected)?;

        // positive case: it is ok for parquet files to be other than upper case
        let sql = "CREATE EXTERNAL TABLE t STORED AS parqueT LOCATION 'foo.parquet'";
        let expected = Statement::CreateExternalTable(CreateExternalTable {
            name: "t".into(),
            columns: vec![],
            file_type: "PARQUET".to_string(),
            has_header: false,
            delimiter: ',',
            location: "foo.parquet".into(),
            table_partition_cols: vec![],
            order_exprs: vec![],
            if_not_exists: false,
            file_compression_type: UNCOMPRESSED,
            options: HashMap::new(),
        });
        expect_parse_ok(sql, expected)?;

        // positive case: it is ok for avro files not to have columns specified
        let sql = "CREATE EXTERNAL TABLE t STORED AS AVRO LOCATION 'foo.avro'";
        let expected = Statement::CreateExternalTable(CreateExternalTable {
            name: "t".into(),
            columns: vec![],
            file_type: "AVRO".to_string(),
            has_header: false,
            delimiter: ',',
            location: "foo.avro".into(),
            table_partition_cols: vec![],
            order_exprs: vec![],
            if_not_exists: false,
            file_compression_type: UNCOMPRESSED,
            options: HashMap::new(),
        });
        expect_parse_ok(sql, expected)?;

        // positive case: it is ok for avro files not to have columns specified
        let sql =
            "CREATE EXTERNAL TABLE IF NOT EXISTS t STORED AS PARQUET LOCATION 'foo.parquet'";
        let expected = Statement::CreateExternalTable(CreateExternalTable {
            name: "t".into(),
            columns: vec![],
            file_type: "PARQUET".to_string(),
            has_header: false,
            delimiter: ',',
            location: "foo.parquet".into(),
            table_partition_cols: vec![],
            order_exprs: vec![],
            if_not_exists: true,
            file_compression_type: UNCOMPRESSED,
            options: HashMap::new(),
        });
        expect_parse_ok(sql, expected)?;

        // Error cases: partition column does not support type
        let sql =
            "CREATE EXTERNAL TABLE t(c1 int) STORED AS CSV PARTITIONED BY (p1 int) LOCATION 'foo.csv'";
        expect_parse_error(sql, "sql parser error: Expected ',' or ')' after partition definition, found: int");

        // positive case: additional options (one entry) can be specified
        let sql =
            "CREATE EXTERNAL TABLE t STORED AS x OPTIONS ('k1' 'v1') LOCATION 'blahblah'";
        let expected = Statement::CreateExternalTable(CreateExternalTable {
            name: "t".into(),
            columns: vec![],
            file_type: "X".to_string(),
            has_header: false,
            delimiter: ',',
            location: "blahblah".into(),
            table_partition_cols: vec![],
            order_exprs: vec![],
            if_not_exists: false,
            file_compression_type: UNCOMPRESSED,
            options: HashMap::from([("k1".into(), "v1".into())]),
        });
        expect_parse_ok(sql, expected)?;

        // positive case: additional options (multiple entries) can be specified
        let sql =
            "CREATE EXTERNAL TABLE t STORED AS x OPTIONS ('k1' 'v1', k2 v2) LOCATION 'blahblah'";
        let expected = Statement::CreateExternalTable(CreateExternalTable {
            name: "t".into(),
            columns: vec![],
            file_type: "X".to_string(),
            has_header: false,
            delimiter: ',',
            location: "blahblah".into(),
            table_partition_cols: vec![],
            order_exprs: vec![],
            if_not_exists: false,
            file_compression_type: UNCOMPRESSED,
            options: HashMap::from([
                ("k1".into(), "v1".into()),
                ("k2".into(), "v2".into()),
            ]),
        });
        expect_parse_ok(sql, expected)?;

        // Ordered Col
        let sqls = vec!["CREATE EXTERNAL TABLE t(c1 int) STORED AS CSV WITH ORDER (c1) LOCATION 'foo.csv'",
                        "CREATE EXTERNAL TABLE t(c1 int) STORED AS CSV WITH ORDER (c1 NULLS FIRST) LOCATION 'foo.csv'",
                        "CREATE EXTERNAL TABLE t(c1 int) STORED AS CSV WITH ORDER (c1 NULLS LAST) LOCATION 'foo.csv'",
                        "CREATE EXTERNAL TABLE t(c1 int) STORED AS CSV WITH ORDER (c1 ASC) LOCATION 'foo.csv'",
                        "CREATE EXTERNAL TABLE t(c1 int) STORED AS CSV WITH ORDER (c1 DESC) LOCATION 'foo.csv'",
                        "CREATE EXTERNAL TABLE t(c1 int) STORED AS CSV WITH ORDER (c1 DESC NULLS FIRST) LOCATION 'foo.csv'",
                        "CREATE EXTERNAL TABLE t(c1 int) STORED AS CSV WITH ORDER (c1 DESC NULLS LAST) LOCATION 'foo.csv'",
                        "CREATE EXTERNAL TABLE t(c1 int) STORED AS CSV WITH ORDER (c1 ASC NULLS FIRST) LOCATION 'foo.csv'",
                        "CREATE EXTERNAL TABLE t(c1 int) STORED AS CSV WITH ORDER (c1 ASC NULLS LAST) LOCATION 'foo.csv'"];
        let expected = vec![
            (None, None),
            (None, Some(true)),
            (None, Some(false)),
            (Some(true), None),
            (Some(false), None),
            (Some(false), Some(true)),
            (Some(false), Some(false)),
            (Some(true), Some(true)),
            (Some(true), Some(false)),
        ];
        for (sql, (asc, nulls_first)) in sqls.iter().zip(expected.into_iter()) {
            let expected = Statement::CreateExternalTable(CreateExternalTable {
                name: "t".into(),
                columns: vec![make_column_def("c1", DataType::Int(None))],
                file_type: "CSV".to_string(),
                has_header: false,
                delimiter: ',',
                location: "foo.csv".into(),
                table_partition_cols: vec![],
                order_exprs: vec![OrderByExpr {
                    expr: Identifier(Ident {
                        value: "c1".to_owned(),
                        quote_style: None,
                    }),
                    asc,
                    nulls_first,
                }],
                if_not_exists: false,
                file_compression_type: UNCOMPRESSED,
                options: HashMap::new(),
            });
            expect_parse_ok(sql, expected)?;
        }

        // Ordered Col
        let sql = "CREATE EXTERNAL TABLE t(c1 int, c2 int) STORED AS CSV WITH ORDER (c1 ASC, c2 DESC NULLS FIRST) LOCATION 'foo.csv'";
        let display = None;
        let expected = Statement::CreateExternalTable(CreateExternalTable {
            name: "t".into(),
            columns: vec![
                make_column_def("c1", DataType::Int(display)),
                make_column_def("c2", DataType::Int(display)),
            ],
            file_type: "CSV".to_string(),
            has_header: false,
            delimiter: ',',
            location: "foo.csv".into(),
            table_partition_cols: vec![],
            order_exprs: vec![
                OrderByExpr {
                    expr: Identifier(Ident {
                        value: "c1".to_owned(),
                        quote_style: None,
                    }),
                    asc: Some(true),
                    nulls_first: None,
                },
                OrderByExpr {
                    expr: Identifier(Ident {
                        value: "c2".to_owned(),
                        quote_style: None,
                    }),
                    asc: Some(false),
                    nulls_first: Some(true),
                },
            ],
            if_not_exists: false,
            file_compression_type: UNCOMPRESSED,
            options: HashMap::new(),
        });
        expect_parse_ok(sql, expected)?;

        // Ordered Binary op
        let sql = "CREATE EXTERNAL TABLE t(c1 int, c2 int) STORED AS CSV WITH ORDER (c1 - c2 ASC) LOCATION 'foo.csv'";
        let display = None;
        let expected = Statement::CreateExternalTable(CreateExternalTable {
            name: "t".into(),
            columns: vec![
                make_column_def("c1", DataType::Int(display)),
                make_column_def("c2", DataType::Int(display)),
            ],
            file_type: "CSV".to_string(),
            has_header: false,
            delimiter: ',',
            location: "foo.csv".into(),
            table_partition_cols: vec![],
            order_exprs: vec![OrderByExpr {
                expr: Expr::BinaryOp {
                    left: Box::new(Identifier(Ident {
                        value: "c1".to_owned(),
                        quote_style: None,
                    })),
                    op: BinaryOperator::Minus,
                    right: Box::new(Identifier(Ident {
                        value: "c2".to_owned(),
                        quote_style: None,
                    })),
                },
                asc: Some(true),
                nulls_first: None,
            }],
            if_not_exists: false,
            file_compression_type: UNCOMPRESSED,
            options: HashMap::new(),
        });
        expect_parse_ok(sql, expected)?;

        // Most complete CREATE EXTERNAL TABLE statement possible
        let sql = "
            CREATE EXTERNAL TABLE IF NOT EXISTS t (c1 int, c2 float)
            STORED AS PARQUET
            DELIMITER '*'
            WITH HEADER ROW
            WITH ORDER (c1 - c2 ASC)
            COMPRESSION TYPE zstd
            PARTITIONED BY (c1)
            LOCATION 'foo.parquet'
            OPTIONS (ROW_GROUP_SIZE '1024', 'TRUNCATE' 'NO')
        ";
        let expected = Statement::CreateExternalTable(CreateExternalTable {
            name: "t".into(),
            columns: vec![
                make_column_def("c1", DataType::Int(None)),
                make_column_def("c2", DataType::Float(None)),
            ],
            file_type: "PARQUET".to_string(),
            has_header: true,
            delimiter: '*',
            location: "foo.parquet".into(),
            table_partition_cols: vec!["c1".into()],
            order_exprs: vec![OrderByExpr {
                expr: Expr::BinaryOp {
                    left: Box::new(Identifier(Ident {
                        value: "c1".to_owned(),
                        quote_style: None,
                    })),
                    op: BinaryOperator::Minus,
                    right: Box::new(Identifier(Ident {
                        value: "c2".to_owned(),
                        quote_style: None,
                    })),
                },
                asc: Some(true),
                nulls_first: None,
            }],
            if_not_exists: true,
            file_compression_type: CompressionTypeVariant::ZSTD,
            options: HashMap::from([
                ("ROW_GROUP_SIZE".into(), "1024".into()),
                ("TRUNCATE".into(), "NO".into()),
            ]),
        });
        expect_parse_ok(sql, expected)?;

        // For error cases, see: `create_external_table.slt`

        Ok(())
    }
}

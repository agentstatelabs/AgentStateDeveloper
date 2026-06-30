//! Canonical per-language conformance fixtures.
//!
//! Each fixture is a single self-contained source file built to exercise the
//! same five capabilities in every language:
//!
//! - a free function/method that **calls another local function** (`process`
//!   → `helper`) — for the `call_edges` probe;
//! - a declared **type** and at least one **function** — for the `symbols`
//!   probe;
//! - an **inbound HTTP route** for `GET /users/{id}` — collapses to the
//!   contract `http:GET /users/{}` in every framework's spelling;
//! - an **outbound HTTP client call** to a URL whose path is `/users/{id}` —
//!   so it normalizes to the *same* contract as the inbound route (this is
//!   what makes the cross-language contract test meaningful);
//! - an effect-bearing call (the HTTP client) for the `effects` probe.
//!
//! Fixtures are deliberately minimal and framework-realistic — just enough to
//! trip each detector. They are NOT meant to compile/run; they are parsed.

/// One language's conformance fixture.
pub struct Fixture {
    /// Matches `LanguageAdapter::language()`.
    pub language: &'static str,
    /// Filename with the real extension (some adapters dispatch on it).
    pub file: &'static str,
    /// Source to parse.
    pub source: &'static str,
}

pub const PYTHON: &str = r#"
import requests
from fastapi import FastAPI

app = FastAPI()


class UserService:
    pass


def helper(x):
    return x + 1


def process(x):
    return helper(x)


@app.get("/users/{id}")
def get_user(id):
    value = process(id)
    resp = requests.get("https://upstream.svc/users/{id}")
    return {"value": value, "resp": resp}
"#;

pub const TYPESCRIPT: &str = r#"
import axios from "axios";
import express from "express";

const app = express();

class UserService {}

function helper(x: number): number {
  return x + 1;
}

function process(x: number): number {
  return helper(x);
}

// Routes are registered inside a function so the detector has an enclosing
// symbol to attribute them to — top-level `app.get(...)` would be dropped
// (same class as Ruby's top-level Rails routes).
function registerRoutes(app: express.Express): void {
  app.get("/users/:id", (req, res) => {
    const value = process(1);
    axios.get("https://upstream.svc/users/:id").then((r) => res.json(r.data));
  });
}
"#;

pub const GO: &str = r#"
package main

import (
	"net/http"

	"github.com/go-chi/chi/v5"
)

type UserService struct{}

func helper(x int) int {
	return x + 1
}

func process(x int) int {
	return helper(x)
}

func GetUser(w http.ResponseWriter, r *http.Request) {
	_ = process(1)
	resp, _ := http.Get("https://upstream.svc/users/{id}")
	_ = resp
}

func routes() {
	r := chi.NewRouter()
	r.Get("/users/{id}", GetUser)
}
"#;

pub const JAVA: &str = r#"
import org.springframework.web.bind.annotation.GetMapping;
import org.springframework.web.bind.annotation.RestController;
import org.springframework.web.client.RestTemplate;

@RestController
public class UserController {

    private int helper(int x) {
        return x + 1;
    }

    private int process(int x) {
        return helper(x);
    }

    @GetMapping("/users/{id}")
    public String getUser() {
        int value = process(1);
        RestTemplate rt = new RestTemplate();
        return rt.getForObject("https://upstream.svc/users/{id}", String.class);
    }
}
"#;

pub const RUBY: &str = r#"
require "sinatra/base"
require "rest-client"

class UserApp < Sinatra::Base
  def helper(x)
    x + 1
  end

  def process(x)
    helper(x)
  end

  get "/users/:id" do
    process(1)
    RestClient.get("https://upstream.svc/users/:id")
  end
end
"#;

pub const CSHARP: &str = r#"
using Microsoft.AspNetCore.Mvc;
using System.Net.Http;

[ApiController]
[Route("[controller]")]
public class UsersController : ControllerBase
{
    private int Helper(int x) => x + 1;

    private int Process(int x) => Helper(x);

    [HttpGet("{id}")]
    public string GetUser()
    {
        int value = Process(1);
        var client = new HttpClient();
        return client.GetAsync("https://upstream.svc/users/{id}").Result.ToString();
    }
}
"#;

pub const KOTLIN: &str = r#"
import org.springframework.web.bind.annotation.GetMapping
import org.springframework.web.bind.annotation.RestController
import org.springframework.web.client.RestTemplate

@RestController
class UserController {
    fun helper(x: Int): Int = x + 1

    fun process(x: Int): Int = helper(x)

    @GetMapping("/users/{id}")
    fun getUser(): String {
        val value = process(1)
        val rt = RestTemplate()
        return rt.getForObject("https://upstream.svc/users/{id}", String::class.java)
    }
}
"#;

pub const SWIFT: &str = r#"
import Vapor

struct UserController {
    func helper(_ x: Int) -> Int {
        return x + 1
    }

    func process(_ x: Int) -> Int {
        return helper(x)
    }

    // URLSession is detected as a network *effect*, but Swift has no
    // cross-service outbound client detector yet — so `outbound` stays `--`
    // while `effects` is `ok`. That asymmetry is the t-016 gap made visible.
    func fetch() {
        let url = URL(string: "https://upstream.svc/users/1")!
        URLSession.shared.dataTask(with: url) { _, _, _ in }.resume()
    }

    func boot(routes: RoutesBuilder) throws {
        routes.get("users", ":id") { req in
            let value = self.process(1)
            return "\(value)"
        }
    }
}
"#;

pub const RUST: &str = r#"
use actix_web::{get, HttpResponse};

struct UserService;

fn helper(x: i32) -> i32 {
    x + 1
}

fn process(x: i32) -> i32 {
    helper(x)
}

#[get("/users/{id}")]
async fn get_user() -> HttpResponse {
    let _value = process(1);
    let _ = reqwest::get("https://upstream.svc/users/{id}").await;
    HttpResponse::Ok().finish()
}
"#;

/// Every fixture, one per built-in adapter language.
pub const ALL: &[Fixture] = &[
    Fixture {
        language: "python",
        file: "sample.py",
        source: PYTHON,
    },
    Fixture {
        language: "typescript",
        file: "sample.ts",
        source: TYPESCRIPT,
    },
    Fixture {
        language: "go",
        file: "sample.go",
        source: GO,
    },
    Fixture {
        language: "java",
        file: "Sample.java",
        source: JAVA,
    },
    Fixture {
        language: "ruby",
        file: "sample.rb",
        source: RUBY,
    },
    Fixture {
        language: "csharp",
        file: "Sample.cs",
        source: CSHARP,
    },
    Fixture {
        language: "kotlin",
        file: "Sample.kt",
        source: KOTLIN,
    },
    Fixture {
        language: "swift",
        file: "Sample.swift",
        source: SWIFT,
    },
    Fixture {
        language: "rust",
        file: "sample.rs",
        source: RUST,
    },
];

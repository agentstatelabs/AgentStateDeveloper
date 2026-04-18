// Pure greeting helpers. No I/O, no logging, no side effects.

export function hello(name: string): string {
    return `Hello, ${name}!`;
}

export function formatWelcome(name: string, locale: string = "en"): string {
    const templates: Record<string, string> = {
        en: "Welcome, {name}.",
        es: "Bienvenido, {name}.",
        fr: "Bienvenue, {name}.",
    };
    const template = templates[locale] ?? templates.en;
    return template.replace("{name}", name);
}

export class Greeter {
    constructor(private salutation: string = "Hello") {}

    greet(name: string): string {
        return `${this.salutation}, ${name}!`;
    }
}
